//! LCM leaf-pass compaction.
//!
//! Loads the oldest `'message'` context items outside the protected
//! tail, groups them into chunks under a per-chunk token budget,
//! summarizes each chunk via the three-level escalator, and replaces
//! the chunk's range in `context_items` with a single `'summary'`
//! item linked to a new leaf node.
//!
//! The condensed pass (depth >= 1) lands separately and reuses the
//! same escalator. This module owns only the leaf phase.
//!
//! See spec 14 §"Two-Phase Compaction".

use std::fmt::Write as _;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use super::engine::{reconstruct_message, storage_err};
use super::summarize::EscalationOutcome;
use crate::error::EngineError;
use crate::types::Message;

/// Protected tail size: most recent N message items are never
/// compacted. Hard-coded; moves to `LcmConfig` once the rest of the
/// compaction config lands.
pub(super) const FRESH_TAIL_COUNT: usize = 32;

/// Maximum tokens per leaf chunk. Hard-coded for now.
pub(super) const LEAF_CHUNK_TOKENS: i64 = 20_000;

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

/// Deterministic summary id: `sum_` + first 16 hex chars of
/// SHA-256(content || sorted source ids).
///
/// Including source ids in the hash makes compaction idempotent under
/// summary content collisions: two distinct chunks that happen to
/// produce identical summary text still get distinct ids.
fn derive_summary_id(content: &str, source_ids: impl IntoIterator<Item = i64>) -> String {
    let mut sorted: Vec<i64> = source_ids.into_iter().collect();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    for id in sorted {
        hasher.update(b"|");
        hasher.update(id.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(20);
    hex.push_str("sum_");
    for byte in &digest[..8] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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
