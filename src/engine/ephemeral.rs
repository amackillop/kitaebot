//! In-memory context for sub-agent turns.
//!
//! See `specs/19-sub-agents.md` for the design.

use std::collections::BTreeMap;
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
pub struct EphemeralSession {
    messages: Vec<Message>,
    /// Tool result contents above this many estimated tokens are
    /// truncated tail-biased at push. Sub-agents exist to absorb
    /// verbose output, so their cap is far above the root's.
    tool_output_tokens: usize,
}

impl EphemeralSession {
    pub fn new(tool_output_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            tool_output_tokens,
        }
    }
}

impl ContextEngine for EphemeralSession {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        let msg = match msg {
            Message::Tool { call_id, content } => {
                let content = match super::truncate_tool_output(&content, self.tool_output_tokens) {
                    std::borrow::Cow::Owned(truncated) => truncated,
                    std::borrow::Cow::Borrowed(_) => content,
                };
                Message::Tool { call_id, content }
            }
            other => other,
        };
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

    fn observe_tokens(&mut self, _prompt_tokens: usize) {
        // Never compacts, so there is no trigger to inform.
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

    async fn report(&self) -> Result<String, EngineError> {
        // Unreachable in practice: children get no slash commands.
        Ok(crate::stats::render(std::slice::from_ref(&self.messages)))
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

    async fn pending_distill_tokens(
        &self,
        _since: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, EngineError> {
        // Single-turn context, never distilled.
        Ok(BTreeMap::new())
    }

    async fn transcript_since(
        &self,
        _session: &str,
        _after: u64,
        _max_tokens: u64,
    ) -> Result<Vec<Message>, EngineError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

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
        let mut engine = EphemeralSession::new(20_000);
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
    async fn tool_output_within_cap_is_untouched() {
        // 20k tokens = 80k bytes; an lcm_expand-sized result (just
        // under the cap) must survive whole.
        let mut engine = EphemeralSession::new(20_000);
        let payload = "y".repeat(79_000);
        engine
            .push_message(Message::Tool {
                call_id: "c1".to_string(),
                content: payload.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(
            &engine.messages[0],
            Message::Tool { content, .. } if *content == payload
        ));
    }

    #[tokio::test]
    async fn tool_output_over_cap_is_truncated() {
        let mut engine = EphemeralSession::new(100);
        engine
            .push_message(Message::Tool {
                call_id: "c1".to_string(),
                content: "z".repeat(10_000),
            })
            .await
            .unwrap();
        assert!(matches!(
            &engine.messages[0],
            Message::Tool { content, .. } if content.contains("tokens truncated")
        ));
    }

    #[tokio::test]
    async fn compaction_is_a_noop() {
        let mut engine = EphemeralSession::new(20_000);
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
    async fn distillation_is_a_noop() {
        let mut engine = EphemeralSession::new(20_000);
        engine
            .push_message(Message::User {
                content: "x".repeat(10_000),
            })
            .await
            .unwrap();

        assert!(
            engine
                .pending_distill_tokens(&BTreeMap::new())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            engine
                .transcript_since("ephemeral", 0, u64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_turn_completes_against_ephemeral_session() {
        let provider = MockProvider::new(vec![Ok(Response::Text("done".to_string()))]);
        let mut engine = EphemeralSession::new(20_000);

        let result = crate::agent::run_turn(
            &mut engine,
            &noop_summarize(),
            "you are a test",
            "hello",
            &provider,
            &Tools::default(),
            5,
            &crate::tools::ToolCtx::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.into_text(), "done");
        // User message plus assistant reply, nothing else.
        assert_eq!(engine.stats().message_count, 2);
    }
}
