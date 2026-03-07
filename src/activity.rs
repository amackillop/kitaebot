//! Structured events emitted during an agent turn.
//!
//! Channels can observe what the agent is doing (tool calls, compaction)
//! without altering the dispatch contract. Events flow through an optional
//! `mpsc::Sender` — callers that don't care pass `None`.

use std::fmt;

use serde::Serialize;
use tokio::sync::mpsc;

/// An observable event during an agent turn.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    /// Context window was compacted via summarization.
    Compaction { before: usize, after: usize },
    /// Agent loop exhausted its iteration budget.
    MaxIterations,
    /// A tool call completed.
    ToolEnd { tool: String, error: Option<String> },
    /// A tool call is about to execute.
    ToolStart { tool: String },
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compaction { before, after } => {
                write!(f, "Compacting context: {before} -> {after} tokens")
            }
            Self::MaxIterations => write!(f, "Max iterations reached"),
            Self::ToolEnd { tool, error: None } => write!(f, "Tool finished: {tool}"),
            Self::ToolEnd {
                tool,
                error: Some(e),
            } => write!(f, "Tool failed: {tool} ({e})"),
            Self::ToolStart { tool } => write!(f, "Running tool: {tool}"),
        }
    }
}

/// Send an event if a sender is present. No-op otherwise.
///
/// Uses `try_send` to stay non-blocking. Events are informational —
/// silently dropped under backpressure is acceptable.
pub fn emit(tx: Option<&mpsc::Sender<Activity>>, event: Activity) {
    if let Some(tx) = tx {
        let _ = tx.try_send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_compaction() {
        let event = Activity::Compaction {
            before: 150_432,
            after: 2841,
        };
        assert_eq!(
            event.to_string(),
            "Compacting context: 150432 -> 2841 tokens"
        );
    }

    #[test]
    fn display_max_iterations() {
        assert_eq!(
            Activity::MaxIterations.to_string(),
            "Max iterations reached"
        );
    }

    #[test]
    fn display_tool_start() {
        let event = Activity::ToolStart {
            tool: "exec".into(),
        };
        assert_eq!(event.to_string(), "Running tool: exec");
    }

    #[test]
    fn display_tool_end_success() {
        let event = Activity::ToolEnd {
            tool: "exec".into(),
            error: None,
        };
        assert_eq!(event.to_string(), "Tool finished: exec");
    }

    #[test]
    fn display_tool_end_failure() {
        let event = Activity::ToolEnd {
            tool: "file_read".into(),
            error: Some("Permission denied".into()),
        };
        assert_eq!(
            event.to_string(),
            "Tool failed: file_read (Permission denied)"
        );
    }

    #[test]
    fn emit_none_is_noop() {
        // Must not panic.
        emit(None, Activity::MaxIterations);
    }

    #[tokio::test]
    async fn emit_sends_event() {
        let (tx, mut rx) = mpsc::channel(4);
        emit(Some(&tx), Activity::MaxIterations);
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, Activity::MaxIterations));
    }

    #[tokio::test]
    async fn emit_drops_on_full_channel() {
        let (tx, _rx) = mpsc::channel(1);
        // Fill the channel.
        emit(Some(&tx), Activity::MaxIterations);
        // Second send should silently drop, not panic.
        emit(Some(&tx), Activity::MaxIterations);
    }
}
