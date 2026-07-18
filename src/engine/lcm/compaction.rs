//! LCM compaction: leaf and condensed passes.
//!
//! The leaf pass collapses the oldest raw messages outside the protected
//! tail into depth-0 summaries. The condensed pass collapses contiguous
//! runs of same-depth summaries into depth+1 summaries. Both reuse the
//! three-level escalator and replace their input range in
//! `context_items` with a single `'summary'` item.
//!
//! See spec 14 §"Two-Phase Compaction".

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tracing::info;

use super::engine::{reconstruct_message, run_blocking, storage_err};
use super::explore::extract_file_ids;
use super::summarize::{EscalationOutcome, summarize_with_escalation};
use crate::config::LcmConfig;
use crate::engine::{CompactionEvent, SummarizeFn};
use crate::error::EngineError;
use crate::types::Message;

/// One eligible row from `context_items` joined with `messages`.
pub(super) struct ChunkRow {
    pub(super) ordinal: i64,
    pub(super) message_id: i64,
    pub(super) token_count: i64,
    pub(super) created_at: String,
    pub(super) message: Message,
}

/// One leaf chunk: a contiguous slice of message context items that
/// will collapse into a single leaf summary.
pub(super) struct LeafChunk {
    pub(super) rows: Vec<ChunkRow>,
}

impl LeafChunk {
    /// The message slice fed to the escalator.
    pub(super) fn messages(&self) -> Vec<Message> {
        self.rows.iter().map(|r| r.message.clone()).collect()
    }
}

/// Load every leaf-eligible chunk for `conversation_id`.
///
/// Eligible = `'message'` items whose ordinal falls outside the last
/// `cfg.fresh_tail_count` message items, except the newest `user`
/// message, which is pinned wherever it sits: a long turn pushes
/// hundreds of tool messages through the tail, and without the pin the
/// task statement is the first thing summarized precisely when the
/// turn still needs its verbatim wording. A newer user message moves
/// the pin. Returns an empty vec when there are too few messages to
/// pull anything out of the protected tail.
///
/// The result is a list of contiguous chunks each summing to no more
/// than `cfg.leaf_chunk_tokens` tokens. The last chunk may be smaller.
pub(super) fn load_leaf_chunks(
    conn: &Connection,
    conversation_id: i64,
    cfg: LcmConfig,
) -> Result<Vec<LeafChunk>, EngineError> {
    let fresh_tail = cfg.fresh_tail_count as usize;
    let leaf_budget = i64::from(cfg.leaf_chunk_tokens);
    let mut stmt = conn
        .prepare(
            "SELECT ci.ordinal, m.message_id, m.role, m.content, \
                    m.token_count, m.created_at \
             FROM context_items ci \
             JOIN messages m ON ci.message_id = m.message_id \
             WHERE ci.conversation_id = ?1 AND ci.item_type = 'message' \
             ORDER BY ci.ordinal",
        )
        .map_err(|e| storage_err(&e))?;

    let raw: Vec<(i64, i64, String, String, i64, String)> = stmt
        .query_map([conversation_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .map_err(|e| storage_err(&e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| storage_err(&e))?;

    if raw.len() <= fresh_tail {
        return Ok(Vec::new());
    }

    let eligible_count = raw.len() - fresh_tail;

    let pinned_ordinal = raw
        .iter()
        .filter(|(_, _, role, ..)| role == "user")
        .map(|(ordinal, ..)| *ordinal)
        .max();

    let mut chunks: Vec<LeafChunk> = Vec::new();
    let mut current = LeafChunk { rows: Vec::new() };
    let mut current_tokens: i64 = 0;

    for (ordinal, message_id, role, content, token_count, created_at) in
        raw.into_iter().take(eligible_count)
    {
        if Some(ordinal) == pinned_ordinal {
            // The write path deletes the chunk's whole ordinal range,
            // so the pin must split the chunk, not merely be skipped.
            if !current.rows.is_empty() {
                chunks.push(std::mem::replace(
                    &mut current,
                    LeafChunk { rows: Vec::new() },
                ));
                current_tokens = 0;
            }
            continue;
        }
        let message = reconstruct_message(conn, message_id, &role, content)?;
        if !current.rows.is_empty() && current_tokens + token_count > leaf_budget {
            chunks.push(std::mem::replace(
                &mut current,
                LeafChunk { rows: Vec::new() },
            ));
            current_tokens = 0;
        }
        current_tokens += token_count;
        current.rows.push(ChunkRow {
            ordinal,
            message_id,
            token_count,
            created_at,
            message,
        });
    }
    if !current.rows.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

/// Persist the result of summarizing a single chunk.
///
/// Inserts the leaf row in `summaries`, links each source message via
/// `summary_messages`, records any `<file>` ids found in the source
/// messages via `summary_files`, then replaces the chunk's
/// `context_items` range with one `'summary'` item placed at the
/// chunk's first ordinal. The whole thing runs in a single
/// transaction so a partial failure cannot leave a half-applied
/// summary.
pub(super) fn write_leaf_summary(
    conn: &mut Connection,
    conversation_id: i64,
    chunk: &LeafChunk,
    outcome: &EscalationOutcome,
) -> Result<(), EngineError> {
    let summary_id = derive_summary_id(&outcome.content, chunk.rows.iter().map(|r| r.message_id));
    let token_count = i64::try_from(outcome.output_tokens).unwrap_or(i64::MAX);
    let descendant_count = i64::try_from(chunk.rows.len()).unwrap_or(i64::MAX);
    let source_tokens: i64 = chunk.rows.iter().map(|r| r.token_count).sum();
    let earliest_at = chunk
        .rows
        .iter()
        .map(|r| r.created_at.as_str())
        .min()
        .unwrap_or("")
        .to_string();
    let latest_at = chunk
        .rows
        .iter()
        .map(|r| r.created_at.as_str())
        .max()
        .unwrap_or("")
        .to_string();
    let first_ordinal = chunk.rows.first().map_or(0, |r| r.ordinal);
    let last_ordinal = chunk.rows.last().map_or(0, |r| r.ordinal);
    let model = outcome.level.tag();

    let tx = conn.transaction().map_err(|e| storage_err(&e))?;

    tx.execute(
        "INSERT INTO summaries \
            (summary_id, conversation_id, kind, depth, content, token_count, \
             earliest_at, latest_at, descendant_count, descendant_token_count, \
             source_message_token_count, model, created_at) \
         VALUES (?1, ?2, 'leaf', 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, datetime('now'))",
        params![
            summary_id,
            conversation_id,
            outcome.content,
            token_count,
            earliest_at,
            latest_at,
            descendant_count,
            source_tokens,
            source_tokens,
            model,
        ],
    )
    .map_err(|e| storage_err(&e))?;

    {
        let mut ins = tx
            .prepare("INSERT INTO summary_messages (summary_id, message_id) VALUES (?1, ?2)")
            .map_err(|e| storage_err(&e))?;
        for row in &chunk.rows {
            ins.execute(params![summary_id, row.message_id])
                .map_err(|e| storage_err(&e))?;
        }
    }

    // Carry `<file>` associations from the source messages onto the
    // summary. INSERT OR IGNORE: several messages in the chunk may
    // reference the same file, and (summary_id, file_id) is the PK.
    // The SELECT filters to ids actually present in `large_files`,
    // since content can mention file-shaped ids that were never
    // externalized and a bare INSERT would abort on the FK.
    {
        let mut ins = tx
            .prepare(
                "INSERT OR IGNORE INTO summary_files (summary_id, file_id) \
                 SELECT ?1, file_id FROM large_files WHERE file_id = ?2",
            )
            .map_err(|e| storage_err(&e))?;
        for row in &chunk.rows {
            for file_id in extract_file_ids(row.message.content()) {
                ins.execute(params![summary_id, file_id])
                    .map_err(|e| storage_err(&e))?;
            }
        }
    }

    tx.execute(
        "DELETE FROM context_items \
         WHERE conversation_id = ?1 AND ordinal BETWEEN ?2 AND ?3",
        params![conversation_id, first_ordinal, last_ordinal],
    )
    .map_err(|e| storage_err(&e))?;
    tx.execute(
        "INSERT INTO context_items \
            (conversation_id, ordinal, item_type, summary_id) \
         VALUES (?1, ?2, 'summary', ?3)",
        params![conversation_id, first_ordinal, summary_id],
    )
    .map_err(|e| storage_err(&e))?;

    tx.commit().map_err(|e| storage_err(&e))?;
    Ok(())
}

/// One eligible row from `context_items` joined with `summaries`.
pub(super) struct CondensedRow {
    pub(super) ordinal: i64,
    pub(super) summary_id: String,
    pub(super) depth: i64,
    pub(super) token_count: i64,
    pub(super) earliest_at: String,
    pub(super) latest_at: String,
    pub(super) descendant_count: i64,
    pub(super) descendant_token_count: i64,
    pub(super) source_message_token_count: i64,
    pub(super) content: String,
}

/// One condensed chunk: a contiguous run of same-depth summary
/// context items that will collapse into a single depth+1 summary.
pub(super) struct CondensedChunk {
    pub(super) rows: Vec<CondensedRow>,
    /// Common depth of every row. The new summary lands at `depth + 1`.
    pub(super) depth: i64,
}

impl CondensedChunk {
    /// Wrap each child summary in a synthetic `<summary>` system
    /// message so the escalator sees structure, not naked prose.
    pub(super) fn messages(&self) -> Vec<Message> {
        self.rows
            .iter()
            .map(|r| Message::System {
                content: format!(
                    "<summary id=\"{}\" depth=\"{}\">\n{}\n</summary>",
                    r.summary_id, r.depth, r.content
                ),
            })
            .collect()
    }
}

/// Load every condensed-eligible chunk for `conversation_id`.
///
/// Walks `context_items` in order and emits one chunk per maximal
/// contiguous run of same-depth summary items where the run has at
/// least `cfg.min_condensed_fanout` members and fits in
/// `cfg.leaf_chunk_tokens`. Runs interrupted by a `'message'` item or
/// a depth change are split. Runs that exceed the token budget are
/// skipped (sub-chunking lands later).
///
/// Returns an empty vec when nothing is eligible, which is also the
/// signal for the caller to stop iterating the condensed pass.
pub(super) fn load_condensed_chunks(
    conn: &Connection,
    conversation_id: i64,
    cfg: LcmConfig,
) -> Result<Vec<CondensedChunk>, EngineError> {
    let min_fanout = cfg.min_condensed_fanout as usize;
    let leaf_budget = i64::from(cfg.leaf_chunk_tokens);
    let mut stmt = conn
        .prepare(
            "SELECT ci.ordinal, ci.item_type, \
                    s.summary_id, s.depth, s.content, s.token_count, \
                    s.earliest_at, s.latest_at, \
                    s.descendant_count, s.descendant_token_count, \
                    s.source_message_token_count \
             FROM context_items ci \
             LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
             WHERE ci.conversation_id = ?1 \
             ORDER BY ci.ordinal",
        )
        .map_err(|e| storage_err(&e))?;

    // None marks a `'message'` item: terminates any in-flight run.
    let raw: Vec<Option<CondensedRow>> = stmt
        .query_map([conversation_id], |r| {
            let ordinal: i64 = r.get(0)?;
            let item_type: String = r.get(1)?;
            if item_type == "message" {
                Ok(None)
            } else {
                Ok(Some(CondensedRow {
                    ordinal,
                    summary_id: r.get(2)?,
                    depth: r.get(3)?,
                    content: r.get(4)?,
                    token_count: r.get(5)?,
                    earliest_at: r.get(6)?,
                    latest_at: r.get(7)?,
                    descendant_count: r.get(8)?,
                    descendant_token_count: r.get(9)?,
                    source_message_token_count: r.get(10)?,
                }))
            }
        })
        .map_err(|e| storage_err(&e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| storage_err(&e))?;

    let mut chunks: Vec<CondensedChunk> = Vec::new();
    let mut current: Vec<CondensedRow> = Vec::new();
    let mut current_depth: Option<i64> = None;
    let mut current_tokens: i64 = 0;

    let flush = |rows: &mut Vec<CondensedRow>,
                 depth: &mut Option<i64>,
                 tokens: &mut i64,
                 out: &mut Vec<CondensedChunk>| {
        if rows.len() >= min_fanout && *tokens <= leaf_budget {
            out.push(CondensedChunk {
                rows: std::mem::take(rows),
                depth: depth.expect("depth set when rows non-empty"),
            });
        } else {
            rows.clear();
        }
        *depth = None;
        *tokens = 0;
    };

    for entry in raw {
        match entry {
            None => flush(
                &mut current,
                &mut current_depth,
                &mut current_tokens,
                &mut chunks,
            ),
            Some(row) => match current_depth {
                Some(d) if d == row.depth => {
                    current_tokens += row.token_count;
                    current.push(row);
                }
                _ => {
                    flush(
                        &mut current,
                        &mut current_depth,
                        &mut current_tokens,
                        &mut chunks,
                    );
                    current_depth = Some(row.depth);
                    current_tokens = row.token_count;
                    current.push(row);
                }
            },
        }
    }
    flush(
        &mut current,
        &mut current_depth,
        &mut current_tokens,
        &mut chunks,
    );

    Ok(chunks)
}

/// Persist the result of summarizing a condensed chunk.
///
/// Inserts the new summary at `depth + 1`, links each child via
/// `summary_parents`, copies the children's `summary_files`
/// associations up to the new node, then replaces the chunk's
/// `context_items` range with one `'summary'` item placed at the
/// chunk's first ordinal. Aggregated descendant counts roll up from
/// children so `lcm_describe` can report total source coverage
/// without walking the DAG.
pub(super) fn write_condensed_summary(
    conn: &mut Connection,
    conversation_id: i64,
    chunk: &CondensedChunk,
    outcome: &EscalationOutcome,
) -> Result<(), EngineError> {
    let summary_id = derive_summary_id_str(
        &outcome.content,
        chunk.rows.iter().map(|r| r.summary_id.as_str()),
    );
    let token_count = i64::try_from(outcome.output_tokens).unwrap_or(i64::MAX);
    let descendant_count: i64 = chunk.rows.iter().map(|r| r.descendant_count).sum();
    let descendant_token_count: i64 = chunk.rows.iter().map(|r| r.descendant_token_count).sum();
    let source_message_token_count: i64 = chunk
        .rows
        .iter()
        .map(|r| r.source_message_token_count)
        .sum();
    let earliest_at = chunk
        .rows
        .iter()
        .map(|r| r.earliest_at.as_str())
        .min()
        .unwrap_or("")
        .to_string();
    let latest_at = chunk
        .rows
        .iter()
        .map(|r| r.latest_at.as_str())
        .max()
        .unwrap_or("")
        .to_string();
    let first_ordinal = chunk.rows.first().map_or(0, |r| r.ordinal);
    let last_ordinal = chunk.rows.last().map_or(0, |r| r.ordinal);
    let new_depth = chunk.depth + 1;
    let model = outcome.level.tag();

    let tx = conn.transaction().map_err(|e| storage_err(&e))?;

    tx.execute(
        "INSERT INTO summaries \
            (summary_id, conversation_id, kind, depth, content, token_count, \
             earliest_at, latest_at, descendant_count, descendant_token_count, \
             source_message_token_count, model, created_at) \
         VALUES (?1, ?2, 'condensed', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))",
        params![
            summary_id,
            conversation_id,
            new_depth,
            outcome.content,
            token_count,
            earliest_at,
            latest_at,
            descendant_count,
            descendant_token_count,
            source_message_token_count,
            model,
        ],
    )
    .map_err(|e| storage_err(&e))?;

    {
        let mut ins = tx
            .prepare("INSERT INTO summary_parents (summary_id, parent_summary_id) VALUES (?1, ?2)")
            .map_err(|e| storage_err(&e))?;
        for row in &chunk.rows {
            ins.execute(params![row.summary_id, summary_id])
                .map_err(|e| storage_err(&e))?;
        }
    }

    // Propagate file associations up the DAG: the new summary covers
    // every file its children covered. Copying rows (rather than
    // re-extracting ids from summary text) keeps the association
    // even if a lossy summarization pass dropped a `<file>` tag.
    {
        let mut ins = tx
            .prepare(
                "INSERT OR IGNORE INTO summary_files (summary_id, file_id) \
                 SELECT ?1, file_id FROM summary_files WHERE summary_id = ?2",
            )
            .map_err(|e| storage_err(&e))?;
        for row in &chunk.rows {
            ins.execute(params![summary_id, row.summary_id])
                .map_err(|e| storage_err(&e))?;
        }
    }

    tx.execute(
        "DELETE FROM context_items \
         WHERE conversation_id = ?1 AND ordinal BETWEEN ?2 AND ?3",
        params![conversation_id, first_ordinal, last_ordinal],
    )
    .map_err(|e| storage_err(&e))?;
    tx.execute(
        "INSERT INTO context_items \
            (conversation_id, ordinal, item_type, summary_id) \
         VALUES (?1, ?2, 'summary', ?3)",
        params![conversation_id, first_ordinal, summary_id],
    )
    .map_err(|e| storage_err(&e))?;

    tx.commit().map_err(|e| storage_err(&e))?;
    Ok(())
}

/// Deterministic summary id: `sum_` + first 16 hex chars of
/// SHA-256(content || sorted source ids).
///
/// Including source ids in the hash makes compaction idempotent under
/// summary content collisions: two distinct chunks that happen to
/// produce identical summary text still get distinct ids.
fn derive_summary_id(content: &str, source_ids: impl IntoIterator<Item = i64>) -> String {
    let mut sorted: Vec<i64> = source_ids.into_iter().collect();
    sorted.sort_unstable();
    derive_summary_id_inner(content, sorted.iter().map(|id| id.to_le_bytes()))
}

/// String-keyed variant for condensed summaries, whose source ids are
/// the child `summary_id` values.
fn derive_summary_id_str<'a>(
    content: &str,
    source_ids: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut sorted: Vec<&str> = source_ids.into_iter().collect();
    sorted.sort_unstable();
    derive_summary_id_inner(content, sorted.iter().map(|s| s.as_bytes()))
}

fn derive_summary_id_inner<K: AsRef<[u8]>>(
    content: &str,
    sorted_keys: impl IntoIterator<Item = K>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    for key in sorted_keys {
        hasher.update(b"|");
        hasher.update(key.as_ref());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(20);
    hex.push_str("sum_");
    for byte in &digest[..8] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Execute one full compaction cycle (leaf pass plus condensed-pass
/// loop) against `conn`, return the before/after token count.
///
/// The blocking path threads the engine's main connection in; the
/// soft-threshold spawn opens a fresh writer connection so the actor's
/// reads on the main mutex proceed unimpeded while this writes.
pub(super) async fn run_compaction(
    conn: Arc<Mutex<Connection>>,
    conversation_id: i64,
    cfg: LcmConfig,
    summarize: &SummarizeFn,
) -> Result<CompactionEvent, EngineError> {
    let before = run_blocking(Arc::clone(&conn), move |c| {
        Ok(token_estimate_sync(c, conversation_id))
    })
    .await?;

    // Leaf pass: collapse oldest raw messages outside the protected
    // tail into depth-0 summaries.
    let leaf_chunks = run_blocking(Arc::clone(&conn), move |c| {
        load_leaf_chunks(c, conversation_id, cfg)
    })
    .await?;

    if !leaf_chunks.is_empty() {
        info!(
            chunk_count = leaf_chunks.len(),
            "running leaf-pass compaction"
        );
        for chunk in leaf_chunks {
            let messages = chunk.messages();
            let outcome = summarize_with_escalation(&messages, summarize).await;
            let c = Arc::clone(&conn);
            run_blocking(c, move |c| {
                write_leaf_summary(c, conversation_id, &chunk, &outcome)
            })
            .await?;
        }
    }

    // Condensed pass: walk the depth ladder. Each iteration collapses
    // contiguous same-depth runs of summaries with fanout >= 2 into a
    // depth+1 summary. Each step strictly reduces the number of
    // summary items in `context_items`, so the loop terminates.
    loop {
        let c = Arc::clone(&conn);
        let chunks =
            run_blocking(c, move |c| load_condensed_chunks(c, conversation_id, cfg)).await?;
        if chunks.is_empty() {
            break;
        }
        info!(
            chunk_count = chunks.len(),
            "running condensed-pass compaction"
        );
        for chunk in chunks {
            let messages = chunk.messages();
            let outcome = summarize_with_escalation(&messages, summarize).await;
            let c = Arc::clone(&conn);
            run_blocking(c, move |c| {
                write_condensed_summary(c, conversation_id, &chunk, &outcome)
            })
            .await?;
        }
    }

    let after = run_blocking(conn, move |c| Ok(token_estimate_sync(c, conversation_id))).await?;

    Ok(CompactionEvent { before, after })
}

/// Sum `token_count` across `context_items` for `conversation_id`,
/// joining both `messages` and `summaries` so the answer covers any
/// mix.
fn token_estimate_sync(conn: &Connection, conversation_id: i64) -> usize {
    let row: rusqlite::Result<i64> = conn.query_row(
        "SELECT COALESCE(SUM(m.token_count), 0) + COALESCE(SUM(s.token_count), 0) \
         FROM context_items ci \
         LEFT JOIN messages  m ON ci.message_id = m.message_id \
         LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
         WHERE ci.conversation_id = ?1",
        [conversation_id],
        |r| r.get(0),
    );
    usize::try_from(row.unwrap_or(0)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lcm::schema;
    use crate::engine::lcm::summarize::EscalationLevel;
    use crate::types::estimate_tokens;

    fn fresh_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = schema::open(&dir.path().join("lcm.db")).unwrap();
        conn.execute(
            "INSERT INTO conversations(name, created_at, updated_at) \
             VALUES ('general', '2025-01-01', '2025-01-01')",
            [],
        )
        .unwrap();
        (dir, conn)
    }

    fn insert_message_with_role(conn: &Connection, seq: i64, role: &str, content: &str) -> i64 {
        conn.execute(
            "INSERT INTO messages(conversation_id, seq, role, content, token_count, created_at) \
             VALUES (1, ?1, ?2, ?3, ?4, '2025-01-01')",
            params![
                seq,
                role,
                content,
                i64::try_from(estimate_tokens(content)).unwrap()
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_message(conn: &Connection, seq: i64, content: &str) -> i64 {
        insert_message_with_role(conn, seq, "user", content)
    }

    fn insert_context_message(conn: &Connection, ordinal: i64, message_id: i64) {
        conn.execute(
            "INSERT INTO context_items(conversation_id, ordinal, item_type, message_id) \
             VALUES (1, ?1, 'message', ?2)",
            params![ordinal, message_id],
        )
        .unwrap();
    }

    fn insert_large_file(conn: &Connection, file_id: &str) {
        conn.execute(
            "INSERT INTO large_files(file_id, conversation_id, path, mime_type, byte_size, \
                                     token_count, exploration_summary, created_at) \
             VALUES (?1, 1, 'p', 'text/plain', 4, 1, 'sum', '2025-01-01')",
            [file_id],
        )
        .unwrap();
    }

    fn outcome(content: &str) -> EscalationOutcome {
        EscalationOutcome {
            content: content.to_string(),
            level: EscalationLevel::Normal,
            input_tokens: 100,
            output_tokens: estimate_tokens(content),
        }
    }

    fn chunk_row(ordinal: i64, message_id: i64, content: &str) -> ChunkRow {
        ChunkRow {
            ordinal,
            message_id,
            token_count: i64::try_from(estimate_tokens(content)).unwrap(),
            created_at: "2025-01-01".into(),
            message: Message::User {
                content: content.into(),
            },
        }
    }

    fn summary_file_pairs(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT summary_id, file_id FROM summary_files ORDER BY summary_id, file_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn leaf_summary_records_referenced_files() {
        let (_dir, mut conn) = fresh_conn();
        insert_large_file(&conn, "file_00000000000000aa");

        // One message references a known file, one references an id
        // that was never externalized: only the known one may land in
        // summary_files (a bare insert would trip the FK).
        let known = "see <file id=\"file_00000000000000aa\" tokens=\"1\">sum</file>";
        let unknown = "mentions file_ffffffffffffffff in passing";
        let m1 = insert_message(&conn, 0, known);
        let m2 = insert_message(&conn, 1, unknown);
        insert_context_message(&conn, 0, m1);
        insert_context_message(&conn, 1, m2);

        let chunk = LeafChunk {
            rows: vec![chunk_row(0, m1, known), chunk_row(1, m2, unknown)],
        };
        write_leaf_summary(&mut conn, 1, &chunk, &outcome("leaf summary")).unwrap();

        let pairs = summary_file_pairs(&conn);
        assert_eq!(pairs.len(), 1, "pairs: {pairs:?}");
        assert_eq!(pairs[0].1, "file_00000000000000aa");
    }

    #[test]
    fn condensed_summary_inherits_child_file_associations() {
        let (_dir, mut conn) = fresh_conn();
        insert_large_file(&conn, "file_00000000000000aa");
        insert_large_file(&conn, "file_00000000000000bb");

        let a = "ref <file id=\"file_00000000000000aa\" tokens=\"1\">s</file>";
        let b = "ref <file id=\"file_00000000000000bb\" tokens=\"1\">s</file>";
        let m1 = insert_message(&conn, 0, a);
        let m2 = insert_message(&conn, 1, b);
        insert_context_message(&conn, 0, m1);
        insert_context_message(&conn, 1, m2);

        write_leaf_summary(
            &mut conn,
            1,
            &LeafChunk {
                rows: vec![chunk_row(0, m1, a)],
            },
            &outcome("leaf a"),
        )
        .unwrap();
        write_leaf_summary(
            &mut conn,
            1,
            &LeafChunk {
                rows: vec![chunk_row(1, m2, b)],
            },
            &outcome("leaf b"),
        )
        .unwrap();

        // Rebuild the two leaves as a condensed chunk.
        let leaves: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT ci.ordinal, s.summary_id, s.content \
                     FROM context_items ci JOIN summaries s ON ci.summary_id = s.summary_id \
                     ORDER BY ci.ordinal",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(leaves.len(), 2);
        let chunk = CondensedChunk {
            rows: leaves
                .into_iter()
                .map(|(ordinal, summary_id, content)| CondensedRow {
                    ordinal,
                    summary_id,
                    depth: 0,
                    token_count: 2,
                    earliest_at: "2025-01-01".into(),
                    latest_at: "2025-01-01".into(),
                    descendant_count: 1,
                    descendant_token_count: 2,
                    source_message_token_count: 2,
                    content,
                })
                .collect(),
            depth: 0,
        };
        write_condensed_summary(&mut conn, 1, &chunk, &outcome("condensed")).unwrap();

        let condensed_id: String = conn
            .query_row(
                "SELECT summary_id FROM summaries WHERE depth = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let files: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT file_id FROM summary_files WHERE summary_id = ?1 ORDER BY file_id")
                .unwrap();
            stmt.query_map([&condensed_id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            files,
            vec!["file_00000000000000aa", "file_00000000000000bb"],
        );
    }

    /// Ordinals 0..n with the given roles, all in context.
    fn seed_conversation(conn: &Connection, roles: &[&str]) {
        for (i, role) in roles.iter().enumerate() {
            let seq = i64::try_from(i).unwrap();
            let id = insert_message_with_role(conn, seq, role, &format!("msg {i}"));
            insert_context_message(conn, seq, id);
        }
    }

    fn chunk_ordinals(chunks: &[LeafChunk]) -> Vec<Vec<i64>> {
        chunks
            .iter()
            .map(|c| c.rows.iter().map(|r| r.ordinal).collect())
            .collect()
    }

    fn cfg_with_tail(fresh_tail_count: u32) -> LcmConfig {
        LcmConfig {
            fresh_tail_count,
            ..LcmConfig::default()
        }
    }

    #[test]
    fn newest_user_message_outside_tail_is_pinned_and_splits_chunks() {
        let (_dir, conn) = fresh_conn();
        // user, assistant, user, assistant, assistant, assistant; tail=2
        // -> eligible ordinals 0..=3, newest user message is ordinal 2.
        seed_conversation(
            &conn,
            &[
                "user",
                "assistant",
                "user",
                "assistant",
                "assistant",
                "assistant",
            ],
        );
        let chunks = load_leaf_chunks(&conn, 1, cfg_with_tail(2)).unwrap();
        // The pin splits the run: [0, 1] and [3]; ordinal 2 is absent so
        // the write path's range delete cannot swallow it.
        assert_eq!(chunk_ordinals(&chunks), vec![vec![0, 1], vec![3]]);
    }

    #[test]
    fn superseded_user_message_is_compactable() {
        let (_dir, conn) = fresh_conn();
        // Two user messages outside the tail: only the newest (1) pins.
        seed_conversation(
            &conn,
            &[
                "user",
                "user",
                "assistant",
                "assistant",
                "assistant",
                "assistant",
            ],
        );
        let chunks = load_leaf_chunks(&conn, 1, cfg_with_tail(2)).unwrap();
        assert_eq!(chunk_ordinals(&chunks), vec![vec![0], vec![2, 3]]);
    }

    #[test]
    fn pin_inside_tail_changes_nothing() {
        let (_dir, conn) = fresh_conn();
        // Newest user message (4) sits inside the protected tail.
        seed_conversation(
            &conn,
            &[
                "assistant",
                "assistant",
                "assistant",
                "assistant",
                "user",
                "assistant",
            ],
        );
        let chunks = load_leaf_chunks(&conn, 1, cfg_with_tail(2)).unwrap();
        assert_eq!(chunk_ordinals(&chunks), vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn summary_id_is_deterministic_and_includes_source_ids() {
        let id_a = derive_summary_id("hello", [1_i64, 2, 3]);
        let id_b = derive_summary_id("hello", [3_i64, 2, 1]);
        assert_eq!(id_a, id_b, "ordering of source ids must not matter");
        assert!(id_a.starts_with("sum_"));
        assert_eq!(id_a.len(), 4 + 16);

        let id_diff_content = derive_summary_id("world", [1_i64, 2, 3]);
        assert_ne!(id_a, id_diff_content);

        let id_diff_sources = derive_summary_id("hello", [1_i64, 2, 4]);
        assert_ne!(id_a, id_diff_sources);
    }
}
