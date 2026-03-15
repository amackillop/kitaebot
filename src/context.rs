#![allow(dead_code)]

//! Context window management.
//!
//! Estimates token usage and compacts the session when it exceeds the
//! configured budget. Compaction summarizes the entire conversation into
//! a single system message via an LLM call, then replaces the history.

use tracing::info;

use crate::config::ContextConfig;
use crate::error::ProviderError;
use crate::provider::Provider;
use crate::session::Session;
use crate::types::{Message, Response};

const SUMMARIZE_PROMPT: &str = "\
You are a conversation summarizer. Produce a concise summary of the \
conversation below. Preserve all important facts, decisions, tool \
results, and open questions. Omit pleasantries and filler. The summary \
will replace the original messages, so nothing important should be lost.";

/// Total estimated tokens for a session, including an external system prompt.
pub fn session_tokens(session: &Session, system_prompt_chars: usize) -> usize {
    let message_chars: usize = session.messages().iter().map(Message::char_count).sum();
    // Crude approximation for English text
    (system_prompt_chars + message_chars) / 4
}

/// Token budget at which compaction triggers.
pub fn budget(config: ContextConfig) -> usize {
    config.max_tokens as usize * usize::from(config.budget_percent) / 100
}

/// Compact the session if estimated tokens exceed the budget.
///
/// Summarizes the entire conversation into a single system message,
/// replacing all existing messages. No-op if under budget or if the
/// session has fewer than 2 messages (nothing meaningful to summarize).
pub async fn compact_if_needed<P: Provider>(
    session: &mut Session,
    system_prompt: &str,
    provider: &P,
    config: ContextConfig,
) -> Result<bool, ProviderError> {
    let tokens = session_tokens(session, system_prompt.len());
    let limit = budget(config);

    if tokens <= limit || session.len() < 2 {
        return Ok(false);
    }

    info!(
        tokens,
        limit,
        messages = session.len(),
        "Compacting context"
    );
    let summary = summarize(session.messages(), provider).await?;
    session.compact(Message::System { content: summary });
    Ok(true)
}

/// Unconditionally run one compaction cycle.
///
/// Returns `false` if the session has fewer than 2 messages (nothing to
/// summarize).
pub async fn force_compact<P: Provider>(
    session: &mut Session,
    provider: &P,
) -> Result<bool, ProviderError> {
    if session.len() < 2 {
        return Ok(false);
    }

    let summary = summarize(session.messages(), provider).await?;
    session.compact(Message::System { content: summary });
    Ok(true)
}

/// Summarize a slice of messages via an LLM call.
async fn summarize<P: Provider>(
    messages: &[Message],
    provider: &P,
) -> Result<String, ProviderError> {
    let prompt_messages = vec![
        Message::System {
            content: SUMMARIZE_PROMPT.to_string(),
        },
        Message::User {
            content: format_messages_for_summary(messages),
        },
    ];

    let response = provider.chat(&prompt_messages, &[]).await?;

    match response {
        Response::Text(text) => Ok(text),
        Response::ToolCalls { content, .. } => {
            // No tools were provided, so this shouldn't happen. Use
            // whatever text content came back.
            Ok(content)
        }
    }
}

fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                out.push_str("[assistant] ");
                out.push_str(content);
                if let Some(calls) = tool_calls {
                    for tc in calls {
                        out.push_str("\n  [tool_call] ");
                        out.push_str(&tc.function.name);
                        out.push('(');
                        out.push_str(&tc.function.arguments);
                        out.push(')');
                    }
                }
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

    fn default_config() -> ContextConfig {
        ContextConfig::default()
    }

    fn tiny_config() -> ContextConfig {
        ContextConfig {
            max_tokens: 100,
            budget_percent: 50,
        }
    }

    #[test]
    fn budget_applies_ratio() {
        let config = ContextConfig {
            max_tokens: 1000,
            budget_percent: 50,
        };
        assert_eq!(budget(config), 500);
    }

    #[test]
    fn session_tokens_includes_system_prompt() {
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "abcd".to_string(), // 4 chars = 1 token
        });
        // system_prompt: 8 chars = 2 tokens, message: 4 chars = 1 token
        // total = 12 chars / 4 = 3 tokens
        assert_eq!(session_tokens(&session, 8), 3);
    }

    #[tokio::test]
    async fn no_compaction_under_budget() {
        let provider = MockProvider::new(vec![]);
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "short".to_string(),
        });

        let compacted = compact_if_needed(&mut session, "sys", &provider, default_config())
            .await
            .unwrap();

        assert!(!compacted);
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn no_compaction_with_fewer_than_two_messages() {
        let provider = MockProvider::new(vec![]);
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "x".repeat(10000),
        });

        let compacted = compact_if_needed(&mut session, "sys", &provider, tiny_config())
            .await
            .unwrap();

        assert!(!compacted);
    }

    #[tokio::test]
    async fn compaction_triggers_over_budget() {
        let provider = MockProvider::new(vec![Ok(Response::Text(
            "Summary of conversation".to_string(),
        ))]);

        let mut session = Session::new();
        // Each message: 200 chars = 50 tokens. Two messages = 100 tokens.
        // tiny_config budget = 100 * 0.5 = 50 tokens. Over budget.
        session.add_message(Message::User {
            content: "a".repeat(200),
        });
        session.add_message(Message::Assistant {
            content: "b".repeat(200),
            tool_calls: None,
        });

        let compacted = compact_if_needed(&mut session, "", &provider, tiny_config())
            .await
            .unwrap();

        assert!(compacted);
        assert_eq!(session.len(), 1);
        assert_eq!(provider.call_count(), 1);
        assert!(
            matches!(&session.messages()[0], Message::System { content } if content == "Summary of conversation")
        );
    }

    #[tokio::test]
    async fn force_compact_runs_unconditionally() {
        let provider = MockProvider::new(vec![Ok(Response::Text("forced".to_string()))]);
        let mut session = Session::new();
        session.add_message(Message::User {
            content: "a".to_string(),
        });
        session.add_message(Message::User {
            content: "b".to_string(),
        });

        let compacted = force_compact(&mut session, &provider).await.unwrap();

        assert!(compacted);
        assert_eq!(session.len(), 1);
        assert!(
            matches!(&session.messages()[0], Message::System { content } if content == "forced")
        );
    }

    #[tokio::test]
    async fn force_compact_skips_empty_session() {
        let provider = MockProvider::new(vec![]);
        let mut session = Session::new();

        let compacted = force_compact(&mut session, &provider).await.unwrap();

        assert!(!compacted);
        assert_eq!(provider.call_count(), 0);
    }
}
