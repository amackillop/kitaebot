//! Flat session implementation of [`ContextEngine`].
//!
//! Wraps `Session` + the compaction logic from `context.rs` behind the
//! trait. Single-session only ("general"). Multi-session comes in Phase 2.

use std::path::PathBuf;

use tracing::info;

use crate::config::ContextConfig;
use crate::error::EngineError;
use crate::session::Session;
use crate::tools::Tool;
use crate::types::Message;

use super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
};

/// Flat session engine -- preserves the pre-engine behavior exactly.
pub struct FlatSession {
    session: Session,
    path: PathBuf,
    ctx: ContextConfig,
}

impl FlatSession {
    /// Load (or create) a flat session from `path`.
    pub fn new(path: PathBuf, ctx: ContextConfig) -> Result<Self, EngineError> {
        let session = Session::load(&path)?;
        Ok(Self { session, path, ctx })
    }

    /// Estimated tokens for the current session content plus a system prompt.
    fn token_estimate(&self, system_prompt_chars: usize) -> usize {
        let message_chars: usize = self
            .session
            .messages()
            .iter()
            .map(Message::char_count)
            .sum();
        (system_prompt_chars + message_chars) / 4
    }

    /// Token budget at which compaction triggers.
    fn budget(&self) -> usize {
        self.ctx.max_tokens as usize * usize::from(self.ctx.budget_percent) / 100
    }

    /// Run one compaction cycle via the summarize callback.
    ///
    /// Returns `None` if the session has fewer than 2 messages.
    async fn do_compact(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        if self.session.len() < 2 {
            return Ok(None);
        }

        let before = self.token_estimate(0);
        let summary = summarize(self.session.messages()).await?;
        self.session.compact(Message::System { content: summary });
        let after = self.token_estimate(0);

        Ok(Some(CompactionEvent { before, after }))
    }
}

impl ContextEngine for FlatSession {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        self.session.add_message(msg);
        Ok(())
    }

    async fn assemble(&self, system_prompt: &str) -> Result<AssembledContext, EngineError> {
        let mut messages = Vec::with_capacity(self.session.len() + 1);
        messages.push(Message::System {
            content: system_prompt.to_string(),
        });
        messages.extend(self.session.messages().iter().cloned());

        Ok(AssembledContext { messages })
    }

    async fn compact_if_needed(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        let tokens = self.token_estimate(0);
        let limit = self.budget();

        if tokens <= limit || self.session.len() < 2 {
            return Ok(None);
        }

        info!(
            tokens,
            limit,
            messages = self.session.len(),
            "Compacting context"
        );
        self.do_compact(summarize).await
    }

    async fn force_compact(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<CompactionEvent, EngineError> {
        match self.do_compact(summarize).await? {
            Some(event) => Ok(event),
            None => Ok(CompactionEvent {
                before: 0,
                after: 0,
            }),
        }
    }

    async fn clear(&mut self) -> Result<(), EngineError> {
        self.session.clear();
        Ok(())
    }

    async fn save(&mut self) -> Result<(), EngineError> {
        self.session.save(&self.path)?;
        Ok(())
    }

    fn stats(&self) -> ContextStats {
        ContextStats {
            token_estimate: self.token_estimate(0),
            budget: self.budget(),
            message_count: self.session.len(),
        }
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        Vec::new()
    }

    #[allow(clippy::unnecessary_literal_bound)] // Trait requires &str tied to &self.
    fn active_session(&self) -> &str {
        "general"
    }

    async fn switch_session(&mut self, _name: &str) -> Result<(), EngineError> {
        // Single-session stub. Multi-session in Phase 2.
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        Ok(vec![SessionInfo {
            name: "general".to_string(),
            message_count: self.session.len(),
            estimated_tokens: self.token_estimate(0),
        }])
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use super::*;

    /// Build a `SummarizeFn` that returns a canned response.
    fn mock_summarize(response: &str) -> SummarizeFn {
        let response = response.to_string();
        Box::new(move |_messages: &[Message]| {
            let response = response.clone();
            Box::pin(async move { Ok(response) })
                as Pin<Box<dyn Future<Output = Result<String, _>> + Send>>
        })
    }

    fn tiny_config() -> ContextConfig {
        ContextConfig {
            max_tokens: 100,
            budget_percent: 50,
        }
    }

    fn temp_engine(ctx: ContextConfig) -> FlatSession {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("session.json");
        FlatSession::new(path, ctx).unwrap()
    }

    #[tokio::test]
    async fn push_and_assemble_roundtrip() {
        let mut engine = temp_engine(ContextConfig::default());

        engine
            .push_message(Message::User {
                content: "hello".to_string(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("system prompt").await.unwrap();

        assert_eq!(ctx.messages.len(), 2);
        assert!(
            matches!(&ctx.messages[0], Message::System { content } if content == "system prompt")
        );
        assert!(matches!(&ctx.messages[1], Message::User { content } if content == "hello"));
    }

    #[tokio::test]
    async fn no_compaction_under_budget() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "short".to_string(),
            })
            .await
            .unwrap();

        let summarize = mock_summarize("unused");
        let result = engine.compact_if_needed(&summarize).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn no_compaction_fewer_than_two_messages() {
        let mut engine = temp_engine(tiny_config());
        engine
            .push_message(Message::User {
                content: "x".repeat(10000),
            })
            .await
            .unwrap();

        let summarize = mock_summarize("unused");
        let result = engine.compact_if_needed(&summarize).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn compaction_triggers_over_budget() {
        let mut engine = temp_engine(tiny_config());
        // 200 chars = 50 tokens each. Two = 100 tokens. Budget = 50.
        engine
            .push_message(Message::User {
                content: "a".repeat(200),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "b".repeat(200),
            })
            .await
            .unwrap();

        let summarize = mock_summarize("Summary of conversation");
        let event = engine.compact_if_needed(&summarize).await.unwrap().unwrap();

        assert!(event.before > event.after);
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn force_compact_runs_unconditionally() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "a".repeat(100),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: "b".repeat(100),
            })
            .await
            .unwrap();

        let summarize = mock_summarize("forced");
        let event = engine.force_compact(&summarize).await.unwrap();

        assert_eq!(engine.stats().message_count, 1);
        assert!(event.before > event.after);
    }

    #[tokio::test]
    async fn force_compact_empty_session() {
        let mut engine = temp_engine(ContextConfig::default());
        let summarize = mock_summarize("unused");
        let event = engine.force_compact(&summarize).await.unwrap();

        assert_eq!(event.before, 0);
        assert_eq!(event.after, 0);
    }

    #[tokio::test]
    async fn clear_resets_session() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "msg".to_string(),
            })
            .await
            .unwrap();
        engine.clear().await.unwrap();

        assert_eq!(engine.stats().message_count, 0);
    }

    #[tokio::test]
    async fn save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let ctx = ContextConfig::default();

        {
            let mut engine = FlatSession::new(path.clone(), ctx).unwrap();
            engine
                .push_message(Message::User {
                    content: "persisted".to_string(),
                })
                .await
                .unwrap();
            engine.save().await.unwrap();
        }

        let engine = FlatSession::new(path, ctx).unwrap();
        assert_eq!(engine.stats().message_count, 1);
    }

    #[test]
    fn stats_reflects_state() {
        let engine = temp_engine(tiny_config());
        let stats = engine.stats();
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.token_estimate, 0);
        assert_eq!(stats.budget, 50); // 100 * 50 / 100
    }

    #[test]
    fn active_session_is_general() {
        let engine = temp_engine(ContextConfig::default());
        assert_eq!(engine.active_session(), "general");
    }

    #[tokio::test]
    async fn list_sessions_returns_single() {
        let engine = temp_engine(ContextConfig::default());
        let sessions = engine.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "general");
    }
}
