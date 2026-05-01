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
use super::summarize::{EscalationOutcome, summarize_with_escalation};
use crate::engine::{CompactionEvent, SummarizeFn};
use crate::error::EngineError;
use crate::types::Message;

/// Protected tail size: most recent N message items are never
/// compacted. Hard-coded; moves to `LcmConfig` once the rest of the
/// compaction config lands.
pub(super) const FRESH_TAIL_COUNT: usize = 32;

/// Maximum tokens per leaf chunk. Hard-coded for now.
pub(super) const LEAF_CHUNK_TOKENS: i64 = 20_000;

/// Minimum number of child summaries required to form a condensed
/// summary. A run of 1 cannot compress further; matches the paper's
/// fanout >= 2 invariant.
pub(super) const MIN_CONDENSED_FANOUT: usize = 2;

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
/// [`FRESH_TAIL_COUNT`] message items. Returns an empty vec when there
/// are too few messages to pull anything out of the protected tail.
///
/// The result is a list of contiguous chunks each summing to no more
/// than [`LEAF_CHUNK_TOKENS`] tokens. The last chunk may be smaller.
pub(super) fn load_leaf_chunks(
    conn: &Connection,
    conversation_id: i64,
) -> Result<Vec<LeafChunk>, EngineError> {
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

    if raw.len() <= FRESH_TAIL_COUNT {
        return Ok(Vec::new());
    }

    let eligible_count = raw.len() - FRESH_TAIL_COUNT;

    let mut chunks: Vec<LeafChunk> = Vec::new();
    let mut current = LeafChunk { rows: Vec::new() };
    let mut current_tokens: i64 = 0;

    for (ordinal, message_id, role, content, token_count, created_at) in
        raw.into_iter().take(eligible_count)
    {
        let message = reconstruct_message(conn, message_id, &role, content)?;
        if !current.rows.is_empty() && current_tokens + token_count > LEAF_CHUNK_TOKENS {
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
/// `summary_messages`, then replaces the chunk's `context_items`
/// range with one `'summary'` item placed at the chunk's first
/// ordinal. The whole thing runs in a single transaction so a partial
/// failure cannot leave a half-applied summary.
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
/// least [`MIN_CONDENSED_FANOUT`] members and fits in
/// [`LEAF_CHUNK_TOKENS`]. Runs interrupted by a `'message'` item or a
/// depth change are split. Runs that exceed the token budget are
/// skipped (sub-chunking lands later).
///
/// Returns an empty vec when nothing is eligible, which is also the
/// signal for the caller to stop iterating the condensed pass.
pub(super) fn load_condensed_chunks(
    conn: &Connection,
    conversation_id: i64,
) -> Result<Vec<CondensedChunk>, EngineError> {
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
        if rows.len() >= MIN_CONDENSED_FANOUT && *tokens <= LEAF_CHUNK_TOKENS {
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
/// `summary_parents`, then replaces the chunk's `context_items`
/// range with one `'summary'` item placed at the chunk's first
/// ordinal. Aggregated descendant counts roll up from children so
/// `lcm_describe` can report total source coverage without walking
/// the DAG.
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
    summarize: &SummarizeFn,
) -> Result<CompactionEvent, EngineError> {
    let before = run_blocking(Arc::clone(&conn), move |c| {
        Ok(token_estimate_sync(c, conversation_id))
    })
    .await?;

    // Leaf pass: collapse oldest raw messages outside the protected
    // tail into depth-0 summaries.
    let leaf_chunks = run_blocking(Arc::clone(&conn), move |c| {
        load_leaf_chunks(c, conversation_id)
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
        let chunks = run_blocking(c, move |c| load_condensed_chunks(c, conversation_id)).await?;
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
