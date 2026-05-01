//! Flat session implementation of [`ContextEngine`].
//!
//! Each session is a separate JSON file under `sessions/<name>.json`.
//! The active session name is persisted to `memory/active_session` so
//! it survives daemon restarts.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::info;

use crate::config::ContextConfig;
use crate::error::EngineError;
use crate::session::Session;
use crate::tools::Tool;
use crate::types::Message;

use super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
};

/// Flat session engine with per-name JSON files.
pub struct FlatSession {
    session: Session,
    active_name: String,
    sessions_dir: PathBuf,
    memory_dir: PathBuf,
    ctx: ContextConfig,
}

impl FlatSession {
    /// Open the flat session engine.
    ///
    /// Reads `memory/active_session` to restore the last active session.
    /// Falls back to `"general"` if the file is missing or unreadable.
    pub fn new(
        sessions_dir: PathBuf,
        memory_dir: PathBuf,
        ctx: ContextConfig,
    ) -> Result<Self, EngineError> {
        let active_name = read_active_session(&memory_dir).unwrap_or_else(|| "general".into());
        let path = session_path(&sessions_dir, &active_name);
        let session = Session::load(&path)?;
        Ok(Self {
            session,
            active_name,
            sessions_dir,
            memory_dir,
            ctx,
        })
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

    /// Path to the JSON file for a given session name.
    fn path_for(&self, name: &str) -> PathBuf {
        session_path(&self.sessions_dir, name)
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
        self.session.save(&self.path_for(&self.active_name))?;
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

    fn active_session(&self) -> &str {
        &self.active_name
    }

    async fn switch_session(&mut self, name: &str) -> Result<(), EngineError> {
        let sanitized = sanitize_name(name);
        if sanitized == self.active_name {
            return Ok(());
        }

        // Save the current session before switching.
        self.save().await?;

        // Load (or create) the target session.
        let path = self.path_for(&sanitized);
        self.session = Session::load(&path)?;
        self.active_name = sanitized;
        persist_active_session(&self.memory_dir, &self.active_name);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        let mut sessions = Vec::new();

        let entries = fs::read_dir(&self.sessions_dir)
            .map_err(|e| EngineError::Storage(format!("read sessions dir: {e}")))?;

        for entry in entries {
            let entry = entry.map_err(|e| EngineError::Storage(format!("read dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = desanitize_name(stem);

            // For the active session, use the in-memory state (avoids re-reading).
            if name == self.active_name {
                sessions.push(SessionInfo {
                    name,
                    message_count: self.session.len(),
                    estimated_tokens: self.token_estimate(0),
                });
            } else if let Ok(s) = Session::load(&path) {
                let chars: usize = s.messages().iter().map(Message::char_count).sum();
                sessions.push(SessionInfo {
                    name,
                    message_count: s.len(),
                    estimated_tokens: chars / 4,
                });
            }
        }

        // If no file exists for the active session yet (new, never saved),
        // make sure it still shows up.
        if !sessions.iter().any(|s| s.name == self.active_name) {
            sessions.push(SessionInfo {
                name: self.active_name.clone(),
                message_count: self.session.len(),
                estimated_tokens: self.token_estimate(0),
            });
        }

        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sessions)
    }
}

// ── Name sanitization ───────────────────────────────────────────────

/// Sanitize a session name for use as a filename.
///
/// `/` becomes `--` so repo-style names like `owner/repo` map to
/// `owner--repo.json`. Null bytes and `..` are stripped entirely.
fn sanitize_name(name: &str) -> String {
    name.replace('\0', "").replace("..", "").replace('/', "--")
}

/// Reverse the sanitization to recover the original name.
fn desanitize_name(stem: &str) -> String {
    stem.replace("--", "/")
}

// ── Active session persistence ──────────────────────────────────────

fn session_path(sessions_dir: &Path, name: &str) -> PathBuf {
    let sanitized = sanitize_name(name);
    sessions_dir.join(format!("{sanitized}.json"))
}

/// Read the active session name from `memory/active_session`.
fn read_active_session(memory_dir: &Path) -> Option<String> {
    let path = memory_dir.join("active_session");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist the active session name atomically.
fn persist_active_session(memory_dir: &Path, name: &str) {
    let path = memory_dir.join("active_session");
    let tmp = memory_dir.join("active_session.tmp");
    if fs::write(&tmp, name).is_ok() {
        let _ = fs::rename(&tmp, &path);
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
        let base = dir.keep();
        let sessions_dir = base.join("sessions");
        let memory_dir = base.join("memory");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::create_dir_all(&memory_dir).unwrap();
        FlatSession::new(sessions_dir, memory_dir, ctx).unwrap()
    }

    fn temp_engine_at(base: &Path, ctx: ContextConfig) -> FlatSession {
        let sessions_dir = base.join("sessions");
        let memory_dir = base.join("memory");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::create_dir_all(&memory_dir).unwrap();
        FlatSession::new(sessions_dir, memory_dir, ctx).unwrap()
    }

    // ── Basic operations (unchanged from Phase 1) ───────────────────

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
        let ctx = ContextConfig::default();

        {
            let mut engine = temp_engine_at(dir.path(), ctx);
            engine
                .push_message(Message::User {
                    content: "persisted".to_string(),
                })
                .await
                .unwrap();
            engine.save().await.unwrap();
        }

        let engine = temp_engine_at(dir.path(), ctx);
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
    fn active_session_defaults_to_general() {
        let engine = temp_engine(ContextConfig::default());
        assert_eq!(engine.active_session(), "general");
    }

    // ── Multi-session tests ─────────────────────────────────────────

    #[tokio::test]
    async fn switch_session_roundtrip() {
        let mut engine = temp_engine(ContextConfig::default());

        // Add a message to "general".
        engine
            .push_message(Message::User {
                content: "in general".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        // Switch to "project-a" and add a message there.
        engine.switch_session("project-a").await.unwrap();
        assert_eq!(engine.active_session(), "project-a");
        assert_eq!(engine.stats().message_count, 0);

        engine
            .push_message(Message::User {
                content: "in project-a".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        // Switch back to "general".
        engine.switch_session("general").await.unwrap();
        assert_eq!(engine.active_session(), "general");
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn switch_session_is_idempotent() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "msg".into(),
            })
            .await
            .unwrap();

        // Switching to the already-active session should be a no-op.
        engine.switch_session("general").await.unwrap();
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let mut engine = temp_engine(ContextConfig::default());

        engine
            .push_message(Message::User {
                content: "general msg".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        engine.switch_session("other").await.unwrap();
        engine
            .push_message(Message::User {
                content: "other msg".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        // Each session has exactly one message.
        assert_eq!(engine.stats().message_count, 1);
        engine.switch_session("general").await.unwrap();
        assert_eq!(engine.stats().message_count, 1);

        // And the content is correct.
        let ctx = engine.assemble("sys").await.unwrap();
        assert!(matches!(&ctx.messages[1], Message::User { content } if content == "general msg"));
    }

    #[tokio::test]
    async fn active_session_persists_across_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextConfig::default();

        {
            let mut engine = temp_engine_at(dir.path(), ctx);
            engine.switch_session("my-project").await.unwrap();
            engine.save().await.unwrap();
        }

        let engine = temp_engine_at(dir.path(), ctx);
        assert_eq!(engine.active_session(), "my-project");
    }

    #[tokio::test]
    async fn list_sessions_enumerates_all() {
        let mut engine = temp_engine(ContextConfig::default());

        engine
            .push_message(Message::User {
                content: "a".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        engine.switch_session("beta").await.unwrap();
        engine.save().await.unwrap();

        let sessions = engine.list_sessions().await.unwrap();
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"general"));
        assert!(names.contains(&"beta"));
    }

    // ── Name sanitization tests ─────────────────────────────────────

    #[test]
    fn sanitize_slashes() {
        assert_eq!(sanitize_name("owner/repo"), "owner--repo");
    }

    #[test]
    fn sanitize_double_dots() {
        // `..` stripped, then `/` becomes `--`.
        assert_eq!(sanitize_name("../evil"), "--evil");
        // "a/../b" -> strip ".." -> "a//b" -> replace "/" -> "a----b"
        assert_eq!(sanitize_name("a/../b"), "a----b");
    }

    #[test]
    fn sanitize_null_bytes() {
        assert_eq!(sanitize_name("foo\0bar"), "foobar");
    }

    #[test]
    fn desanitize_reverses_slashes() {
        assert_eq!(desanitize_name("owner--repo"), "owner/repo");
    }

    #[test]
    fn sanitize_roundtrip() {
        let name = "owner/repo";
        assert_eq!(desanitize_name(&sanitize_name(name)), name);
    }

    #[test]
    fn sanitize_plain_name_unchanged() {
        assert_eq!(sanitize_name("general"), "general");
    }
}
