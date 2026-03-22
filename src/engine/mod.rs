//! Context engine abstraction.
//!
//! All context management flows through the [`ContextEngine`] trait. The agent
//! loop, actor, and channels interact exclusively with this interface.
//!
//! Two implementations exist:
//! - **Flat session** (`flat.rs`): wraps `Session` + `context.rs`. No `SQLite`.
//! - **LCM** (future): hierarchical DAG of summaries over `SQLite`.

// Types defined here are consumed starting in commit 1.2 (FlatSession impl).
#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{EngineError, ProviderError};
use crate::provider::Provider;
use crate::tools::Tool;
use crate::types::{Message, Response};

/// Callback for LLM summarization during compaction.
///
/// The engine does not own a provider — it borrows summarization capability
/// via this closure. Constructed once at startup via [`make_summarize_fn`],
/// then passed by reference into compaction methods.
pub type SummarizeFn = Box<
    dyn Fn(&[Message]) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>
        + Send
        + Sync,
>;

/// Everything the agent loop needs for a provider call.
pub struct AssembledContext {
    /// System prompt, possibly augmented by the engine (e.g. LCM recall guidance).
    pub system_prompt: String,
    /// Ordered messages for the provider.
    pub messages: Vec<Message>,
}

/// Compaction event for activity reporting.
pub struct CompactionEvent {
    /// Estimated tokens before compaction.
    pub before: usize,
    /// Estimated tokens after compaction.
    pub after: usize,
}

/// Context statistics.
pub struct ContextStats {
    /// Estimated token count of current context.
    pub token_estimate: usize,
    /// Token budget (compaction trigger threshold).
    pub budget: usize,
    /// Number of messages in current session.
    pub message_count: usize,
}

/// Metadata about a session.
pub struct SessionInfo {
    pub name: String,
    pub message_count: usize,
    pub estimated_tokens: usize,
}

/// Context management trait.
///
/// All methods are async (RPIT). The agent loop is generic over this trait,
/// monomorphized at the call site. One engine per agent — generics, not dyn.
pub trait ContextEngine: Send + Sync {
    /// Append a message to the active session.
    fn push_message(
        &mut self,
        msg: Message,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Assemble the full context for a provider call.
    fn assemble(
        &self,
        system_prompt: &str,
    ) -> impl Future<Output = Result<AssembledContext, EngineError>> + Send;

    /// Compact if estimated tokens exceed the budget. Returns `None` if no
    /// compaction was needed.
    fn compact_if_needed(
        &mut self,
        summarize: &SummarizeFn,
    ) -> impl Future<Output = Result<Option<CompactionEvent>, EngineError>> + Send;

    /// Unconditionally run one compaction cycle.
    fn force_compact(
        &mut self,
        summarize: &SummarizeFn,
    ) -> impl Future<Output = Result<CompactionEvent, EngineError>> + Send;

    /// Clear the active session's history.
    fn clear(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Persist the active session to durable storage.
    fn save(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Current context statistics.
    fn stats(&self) -> ContextStats;

    /// Tools contributed by this engine (empty for flat session).
    fn tools(&self) -> Vec<Box<dyn Tool>>;

    /// Name of the active session.
    fn active_session(&self) -> &str;

    /// Switch to a named session. Creates it if it does not exist.
    fn switch_session(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// List all available sessions.
    fn list_sessions(&self) -> impl Future<Output = Result<Vec<SessionInfo>, EngineError>> + Send;
}

// ── Summarize prompt ─────────────────────────────────────────────────

const SUMMARIZE_PROMPT: &str = "\
You are a conversation summarizer. Produce a concise summary of the \
conversation below. Preserve all important facts, decisions, tool \
results, and open questions. Omit pleasantries and filler. The summary \
will replace the original messages, so nothing important should be lost.";

/// Build a `SummarizeFn` that uses the given provider for LLM calls.
///
/// The provider is captured by `Arc` — one heap allocation, paid once.
pub fn make_summarize_fn<P: Provider + 'static>(provider: Arc<P>) -> SummarizeFn {
    Box::new(move |messages: &[Message]| {
        let provider = provider.clone();
        let prompt_messages = vec![
            Message::System {
                content: SUMMARIZE_PROMPT.to_string(),
            },
            Message::User {
                content: format_messages_for_summary(messages),
            },
        ];

        Box::pin(async move {
            let response = provider.chat(&prompt_messages, &[]).await?;
            match response {
                Response::Text(text) => Ok(text),
                Response::ToolCalls { content, .. } => Ok(content),
            }
        })
    })
}

fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::Assistant { content } => {
                out.push_str("[assistant] ");
                out.push_str(content);
            }
            Message::System { content } => {
                out.push_str("[system] ");
                out.push_str(content);
            }
            Message::Tool { call_id, content } => {
                out.push_str("[tool:");
                out.push_str(call_id);
                out.push_str("] ");
                out.push_str(content);
            }
            Message::ToolCalls { content, calls } => {
                out.push_str("[assistant] ");
                out.push_str(content);
                for tc in calls {
                    out.push_str("\n  [tool_call] ");
                    out.push_str(&tc.function.name);
                    out.push('(');
                    out.push_str(&tc.function.arguments);
                    out.push(')');
                }
            }
            Message::User { content } => {
                out.push_str("[user] ");
                out.push_str(content);
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::types::Response;

    #[tokio::test]
    async fn summarize_fn_calls_provider() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "summary".to_string(),
        ))]));
        let summarize = make_summarize_fn(provider.clone());

        let messages = vec![
            Message::User {
                content: "hello".to_string(),
            },
            Message::Assistant {
                content: "hi".to_string(),
            },
        ];

        let result = summarize(&messages).await.unwrap();
        assert_eq!(result, "summary");
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn summarize_fn_handles_tool_calls_response() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::ToolCalls {
            content: "fallback text".to_string(),
            calls: vec![],
        })]));
        let summarize = make_summarize_fn(provider);

        let result = summarize(&[]).await.unwrap();
        assert_eq!(result, "fallback text");
    }

    #[tokio::test]
    async fn summarize_fn_propagates_error() {
        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::RateLimited)]));
        let summarize = make_summarize_fn(provider);

        let result = summarize(&[]).await;
        assert!(matches!(result, Err(ProviderError::RateLimited)));
    }

    #[test]
    fn format_messages_covers_all_variants() {
        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::User {
                content: "usr".to_string(),
            },
            Message::Assistant {
                content: "ast".to_string(),
            },
            Message::ToolCalls {
                content: "thinking".to_string(),
                calls: vec![crate::types::ToolCall::new(
                    "c1".to_string(),
                    crate::types::ToolFunction {
                        name: "exec".to_string(),
                        arguments: r#"{"cmd":"ls"}"#.to_string(),
                    },
                )],
            },
            Message::Tool {
                call_id: "c1".to_string(),
                content: "file.txt".to_string(),
            },
        ];

        let formatted = format_messages_for_summary(&messages);
        assert!(formatted.contains("[system] sys"));
        assert!(formatted.contains("[user] usr"));
        assert!(formatted.contains("[assistant] ast"));
        assert!(formatted.contains("[assistant] thinking"));
        assert!(formatted.contains("[tool_call] exec"));
        assert!(formatted.contains("[tool:c1] file.txt"));
    }
}
