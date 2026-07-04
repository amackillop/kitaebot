//! In-memory context for sub-agent turns.
//!
//! See `specs/19-sub-agents.md` for the design.

use std::sync::Arc;

use crate::error::EngineError;
use crate::tools::Tool;
use crate::types::Message;

use super::lcm::summarize::estimate_messages_tokens;
use super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
    ToolScope,
};

/// A `Vec<Message>` posing as a context engine.
///
/// One instance backs one sub-agent turn: created by the `task` tool,
/// discarded when the turn ends. Nothing persists and nothing
/// compacts — a child that outgrows the provider's window should fail
/// (surfaced to the parent as tool error text), not quietly summarize
/// away the work it was delegated.
#[derive(Default)]
#[allow(dead_code)] // First caller lands with the task tool (spec 19).
pub struct EphemeralSession {
    messages: Vec<Message>,
}

impl EphemeralSession {
    #[allow(dead_code)] // First caller lands with the task tool (spec 19).
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContextEngine for EphemeralSession {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        self.messages.push(msg);
        Ok(())
    }

    async fn assemble(&self, system_prompt: &str) -> Result<AssembledContext, EngineError> {
        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        messages.push(Message::System {
            content: system_prompt.to_string(),
        });
        messages.extend(self.messages.iter().cloned());
        Ok(AssembledContext { messages })
    }

    async fn compact_if_needed(
        &mut self,
        _summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        Ok(None)
    }

    async fn force_compact(
        &mut self,
        _summarize: &SummarizeFn,
    ) -> Result<CompactionEvent, EngineError> {
        // Unreachable in practice: slash commands never target a child
        // context. Reported as a zero-delta cycle.
        let tokens = estimate_messages_tokens(&self.messages);
        Ok(CompactionEvent {
            before: tokens,
            after: tokens,
        })
    }

    async fn clear(&mut self) -> Result<(), EngineError> {
        self.messages.clear();
        Ok(())
    }

    async fn save(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn stats(&self) -> ContextStats {
        ContextStats {
            token_estimate: estimate_messages_tokens(&self.messages),
            // Never compacts; the real bound is the provider's window.
            budget: usize::MAX,
            message_count: self.messages.len(),
        }
    }

    fn tools(&self, _scope: ToolScope) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    // The trait ties the lifetime to &self; the literal is incidental.
    #[allow(clippy::unnecessary_literal_bound)]
    fn active_session(&self) -> &str {
        "ephemeral"
    }

    async fn switch_session(&mut self, _name: &str) -> Result<(), EngineError> {
        // Meaningless for a single-turn context; ignored.
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response;

    fn noop_summarize() -> SummarizeFn {
        Arc::new(|_prompt: &str, _messages: &[Message]| {
            Box::pin(async { Ok(String::new()) })
                as Pin<Box<dyn Future<Output = Result<String, _>> + Send>>
        })
    }

    #[tokio::test]
    async fn assemble_prepends_system_prompt_in_order() {
        let mut engine = EphemeralSession::new();
        engine
            .push_message(Message::User {
                content: "one".to_string(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "two".to_string(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("sys").await.unwrap();
        assert_eq!(ctx.messages.len(), 3);
        assert!(matches!(&ctx.messages[0], Message::System { content } if content == "sys"));
        assert!(matches!(&ctx.messages[1], Message::User { content } if content == "one"));
        assert!(matches!(&ctx.messages[2], Message::Assistant { content } if content == "two"));
    }

    #[tokio::test]
    async fn compaction_is_a_noop() {
        let mut engine = EphemeralSession::new();
        engine
            .push_message(Message::User {
                content: "x".repeat(10_000),
            })
            .await
            .unwrap();

        let event = engine.compact_if_needed(&noop_summarize()).await.unwrap();
        assert!(event.is_none());
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn run_turn_completes_against_ephemeral_session() {
        let provider = MockProvider::new(vec![Ok(Response::Text("done".to_string()))]);
        let mut engine = EphemeralSession::new();

        let result = crate::agent::run_turn(
            &mut engine,
            &noop_summarize(),
            "you are a test",
            "hello",
            &provider,
            &Tools::default(),
            5,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, "done");
        // User message plus assistant reply, nothing else.
        assert_eq!(engine.stats().message_count, 2);
    }
}
