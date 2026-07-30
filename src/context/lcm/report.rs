//! `/stats` report for the LCM engine.
//!
//! Reconstructs every conversation's raw message history (the
//! `messages` table, not just the live context — compaction must not
//! hide usage from the report) and feeds it through the shared
//! `stats::render` core, then appends an LCM health section built
//! from the store: summary counts by depth, failed summarizations,
//! per-conversation compression, and externalized payloads.

use std::fmt::Write as _;

use rusqlite::Connection;

use crate::error::EngineError;
use crate::types::Message;

use super::super::names::desanitize_name;
use super::engine::{reconstruct_message, storage_err};

/// Build the full `/stats` report: shared tool tables + health section.
pub(super) fn report_sync(conn: &Connection) -> Result<String, EngineError> {
    let sessions = all_conversation_messages(conn)?;
    let mut out = crate::context::stats::render(&sessions);
    out.push_str(&health_section(conn)?);
    Ok(out)
}

/// Reconstruct the raw message history of every conversation.
fn all_conversation_messages(conn: &Connection) -> Result<Vec<Vec<Message>>, EngineError> {
    let ids: Vec<i64> = conn
        .prepare("SELECT conversation_id FROM conversations ORDER BY name")
        .map_err(|e| storage_err(&e))?
        .query_map([], |r| r.get(0))
        .map_err(|e| storage_err(&e))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| storage_err(&e))?;

    let mut sessions = Vec::with_capacity(ids.len());
    for id in ids {
        let rows: Vec<(i64, String, String)> = conn
            .prepare(
                "SELECT message_id, role, content FROM messages \
                 WHERE conversation_id = ?1 ORDER BY seq",
            )
            .map_err(|e| storage_err(&e))?
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| storage_err(&e))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| storage_err(&e))?;

        let mut messages = Vec::with_capacity(rows.len());
        for (message_id, role, content) in rows {
            messages.push(reconstruct_message(conn, message_id, &role, content)?);
        }
        sessions.push(messages);
    }
    Ok(sessions)
}

/// LCM-specific health metrics appended after the shared tables.
fn health_section(conn: &Connection) -> Result<String, EngineError> {
    let mut out = String::from("\nLCM Health\n\n");

    // Summary DAG shape.
    let depths: Vec<(i64, i64)> = conn
        .prepare("SELECT depth, COUNT(*) FROM summaries GROUP BY depth ORDER BY depth")
        .map_err(|e| storage_err(&e))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| storage_err(&e))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| storage_err(&e))?;
    if depths.is_empty() {
        out.push_str("Summaries: none\n");
    } else {
        out.push_str("Summaries by depth:\n");
        for (depth, count) in depths {
            writeln!(out, "  depth {depth}: {count}").unwrap();
        }
    }

    // Level-3 escalations store 'level3-truncate' in the model column.
    let truncated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM summaries WHERE model = 'level3-truncate'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| storage_err(&e))?;
    writeln!(out, "Failed summarizations (truncated): {truncated}").unwrap();

    // Raw stored tokens vs what assemble would send today.
    let compression: Vec<(String, i64, i64)> = conn
        .prepare(
            "SELECT c.name, \
                    (SELECT COALESCE(SUM(token_count), 0) FROM messages \
                     WHERE conversation_id = c.conversation_id), \
                    (SELECT COALESCE(SUM(m.token_count), 0) \
                          + COALESCE(SUM(s.token_count), 0) \
                     FROM context_items ci \
                     LEFT JOIN messages  m ON ci.message_id = m.message_id \
                     LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
                     WHERE ci.conversation_id = c.conversation_id) \
             FROM conversations c ORDER BY c.name",
        )
        .map_err(|e| storage_err(&e))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| storage_err(&e))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| storage_err(&e))?;

    writeln!(
        out,
        "\n{:<20} {:>12}   {:>14}",
        "Conversation", "Raw Tokens", "Context Tokens"
    )
    .unwrap();
    writeln!(
        out,
        "{:<20} {:>12}   {:>14}",
        "------------", "----------", "--------------"
    )
    .unwrap();
    for (name, raw, context) in compression {
        writeln!(
            out,
            "{:<20} {:>12}   {:>14}",
            desanitize_name(&name),
            raw,
            context,
        )
        .unwrap();
    }

    // Externalized payloads.
    let (file_count, file_bytes): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM large_files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| storage_err(&e))?;
    let file_bytes = u64::try_from(file_bytes).unwrap_or(0);
    writeln!(
        out,
        "\nLarge files: {file_count} ({})",
        crate::context::stats::format_bytes(file_bytes),
    )
    .unwrap();

    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use super::super::LcmEngine;
    use super::super::schema;
    use super::*;
    use crate::config::ContextConfig;
    use crate::context::{ContextEngine, SummarizeFn};
    use crate::types::{ToolCall, ToolFunction};

    fn canned_summarize(text: &str) -> SummarizeFn {
        let text = text.to_string();
        Arc::new(move |_prompt: &str, _messages: &[Message]| {
            let text = text.clone();
            Box::pin(async move { Ok(text) })
                as Pin<Box<dyn Future<Output = Result<String, _>> + Send>>
        })
    }

    #[tokio::test]
    async fn report_covers_raw_history_and_health() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ContextConfig::default();
        // 10-token threshold so a small payload externalizes.
        ctx.lcm.large_file_threshold = 10;
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut engine = LcmEngine::new(
            &dir.path().join("lcm.db"),
            state_dir,
            ctx,
            canned_summarize("summary"),
        )
        .unwrap();

        engine
            .push_message(Message::ToolCalls {
                content: String::new(),
                calls: vec![ToolCall::new(
                    "c1".into(),
                    ToolFunction {
                        name: "exec".into(),
                        arguments: r#"{"command":"git status"}"#.into(),
                    },
                )],
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: "clean".into(),
            })
            .await
            .unwrap();
        // Over-threshold user payload -> large_files row.
        engine
            .push_message(Message::User {
                content: "x".repeat(200),
            })
            .await
            .unwrap();

        let report = engine.report().await.unwrap();
        assert!(report.contains("Tool Usage (1 session)"), "{report}");
        assert!(report.contains("exec"));
        assert!(report.contains("git status"));
        assert!(report.contains("LCM Health"));
        assert!(report.contains("Summaries: none"));
        assert!(report.contains("Large files: 1"));
        assert!(report.contains("general"));
    }

    #[test]
    fn health_counts_summaries_and_truncations() {
        let dir = tempfile::tempdir().unwrap();
        let conn = schema::open(&dir.path().join("lcm.db")).unwrap();
        conn.execute(
            "INSERT INTO conversations (name, created_at, updated_at) \
             VALUES ('general', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        let insert_summary = |id: &str, depth: i64, model: &str| {
            conn.execute(
                "INSERT INTO summaries ( \
                     summary_id, conversation_id, kind, depth, content, \
                     token_count, earliest_at, latest_at, descendant_count, \
                     descendant_token_count, source_message_token_count, \
                     model, created_at) \
                 VALUES (?1, 1, 'leaf', ?2, 'text', 10, datetime('now'), \
                         datetime('now'), 2, 100, 100, ?3, datetime('now'))",
                rusqlite::params![id, depth, model],
            )
            .unwrap();
        };
        insert_summary("s1", 1, "test-model");
        insert_summary("s2", 1, "level3-truncate");
        insert_summary("s3", 2, "test-model");

        let out = report_sync(&conn).unwrap();
        assert!(out.contains("depth 1: 2"), "{out}");
        assert!(out.contains("depth 2: 1"));
        assert!(out.contains("Failed summarizations (truncated): 1"));
    }
}
