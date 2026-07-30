//! Flat session implementation of [`ContextEngine`].
//!
//! The engine owns everything under the workspace's `context/`
//! directory: one JSON file per session in `context/sessions/`, and
//! the active-session cursor at `context/active_session` so it
//! survives daemon restarts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info;

use super::names::{desanitize_name, sanitize_name};
use crate::config::ContextConfig;
use crate::error::EngineError;
use crate::session::Session;
use crate::tools::Tool;
use crate::types::{Message, estimate_tokens_from_chars};

use super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
    ToolScope,
};

/// Instruction block used when compacting a flat session into a
/// single summary message. Sent in the user turn by `make_summarize_fn`;
/// the role-setting system prompt lives there. LCM uses its own
/// per-level instruction blocks; this one is flat-only and stays here.
///
/// No `Expand for details about: ...` trailer (flat has no DAG to
/// expand into). The `Files:` line is kept because flat sessions
/// still benefit from explicit file-operation tracking on read-back.
const FLAT_SUMMARIZE_PROMPT: &str = "\
Produce a concise summary of the conversation below. Preserve all \
important facts, decisions, tool results, and open questions. Omit \
pleasantries and filler. The summary will replace the original \
messages, so nothing important should be lost.

Output requirements:
- Plain text only. No preamble, headings, or markdown formatting.
- Include a \"Files:\" line tracking file operations (created, \
modified, deleted, renamed). Each entry: the path plus a short clause \
on why it matters, e.g. \"src/exec.rs (modified: added retry wrapper)\".
- If no file operations appear, include exactly: \"Files: none\".";

/// Flat session engine with per-name JSON files.
pub struct FlatSession {
    session: Session,
    active_name: String,
    sessions_dir: PathBuf,
    context_dir: PathBuf,
    ctx: ContextConfig,
    /// Provider-reported prompt size of the last request, if any.
    /// Cleared whenever the context shrinks (compaction, clear,
    /// session switch) — a stale high-water mark would re-trigger
    /// compaction forever via `max()`.
    observed_tokens: Option<usize>,
}

impl FlatSession {
    /// Open the flat session engine inside `context_dir`, which the
    /// engine owns: sessions land in `context_dir/sessions/` and the
    /// active-session cursor beside them. Reads the cursor to restore
    /// the last active session, falling back to `"general"`.
    pub fn new(context_dir: PathBuf, ctx: ContextConfig) -> Result<Self, EngineError> {
        let sessions_dir = context_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)
            .map_err(|e| EngineError::Storage(format!("create {}: {e}", sessions_dir.display())))?;
        let active_name = read_active_session(&context_dir).unwrap_or_else(|| "general".into());
        let path = session_path(&sessions_dir, &active_name);
        let session = Session::load(&path)?;
        Ok(Self {
            session,
            active_name,
            sessions_dir,
            context_dir,
            ctx,
            observed_tokens: None,
        })
    }

    /// Best available token count: the char-based estimate or the
    /// provider-observed prompt size, whichever is larger.
    fn current_tokens(&self) -> usize {
        self.token_estimate(0)
            .max(self.observed_tokens.unwrap_or(0))
    }

    /// Estimated tokens for the current session content plus a system prompt.
    fn token_estimate(&self, system_prompt_chars: usize) -> usize {
        let message_chars: usize = self
            .session
            .messages()
            .iter()
            .map(Message::char_count)
            .sum();
        estimate_tokens_from_chars(system_prompt_chars + message_chars)
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

        let before = self.current_tokens();
        let summary = summarize(FLAT_SUMMARIZE_PROMPT, self.session.messages()).await?;
        self.session.compact(Message::System { content: summary });
        self.observed_tokens = None;
        let after = self.token_estimate(0);

        Ok(Some(CompactionEvent { before, after }))
    }

    /// Path to the JSON file for a given session name.
    fn path_for(&self, name: &str) -> PathBuf {
        session_path(&self.sessions_dir, name)
    }

    /// A session's messages: in-memory when active (sees unsaved
    /// pushes), otherwise loaded from disk. An unreadable file yields
    /// an empty span rather than failing the whole distill pass.
    fn session_messages(&self, name: &str) -> Vec<Message> {
        if name == self.active_name {
            self.session.messages().to_vec()
        } else {
            Session::load(&self.path_for(name))
                .map(|s| s.messages().to_vec())
                .unwrap_or_default()
        }
    }
}

impl ContextEngine for FlatSession {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        // The flat engine cannot externalize to disk, so oversized
        // tool output is truncated tail-biased instead (LCM keeps it
        // lossless; see spec 14).
        let msg = match msg {
            Message::Tool { call_id, content } => {
                let content = match super::truncate_tool_output(
                    &content,
                    self.ctx.tool_output_tokens as usize,
                ) {
                    std::borrow::Cow::Owned(truncated) => truncated,
                    std::borrow::Cow::Borrowed(_) => content,
                };
                Message::Tool { call_id, content }
            }
            other => other,
        };
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

    fn observe_tokens(&mut self, prompt_tokens: usize) {
        self.observed_tokens = Some(prompt_tokens);
    }

    async fn compact_if_needed(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        let tokens = self.current_tokens();
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
        self.observed_tokens = None;
        Ok(())
    }

    async fn save(&mut self) -> Result<(), EngineError> {
        self.session.save(&self.path_for(&self.active_name))?;
        Ok(())
    }

    fn stats(&self) -> ContextStats {
        ContextStats {
            token_estimate: self.current_tokens(),
            budget: self.budget(),
            message_count: self.session.len(),
        }
    }

    fn tools(&self, _scope: ToolScope) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    async fn report(&self) -> Result<String, EngineError> {
        let mut sessions = Vec::new();
        let mut saw_active = false;

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

            // For the active session, use the in-memory state (avoids
            // re-reading and sees unsaved messages). Both sides are the
            // sanitized form; desanitizing here never matched a name
            // containing a slash, so repo sessions counted twice.
            if stem == self.active_name {
                sessions.push(self.session.messages().to_vec());
                saw_active = true;
            } else if let Ok(s) = Session::load(&path) {
                sessions.push(s.messages().to_vec());
            }
        }

        // Active session with no file yet (new, never saved).
        if !saw_active {
            sessions.push(self.session.messages().to_vec());
        }

        Ok(super::stats::render(&sessions))
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
        self.observed_tokens = None;
        self.active_name = sanitized;
        persist_active_session(&self.context_dir, &self.active_name);
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
                    estimated_tokens: estimate_tokens_from_chars(chars),
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

    async fn latest_positions(&self) -> Result<BTreeMap<String, u64>, EngineError> {
        let mut out = BTreeMap::new();
        for info in self.list_sessions().await? {
            let len = self.session_messages(&info.name).len();
            if len > 0 {
                out.insert(info.name, u64::try_from(len).unwrap_or(u64::MAX));
            }
        }
        Ok(out)
    }

    async fn pending_distill_tokens(
        &self,
        since: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, EngineError> {
        // Positions are message indices, which reset on the flat
        // engine's destructive compaction: a session distilled then
        // compacted can under-report. Best-effort by design (spec 21);
        // LCM is the sound path.
        let mut out = BTreeMap::new();
        for info in self.list_sessions().await? {
            let msgs = self.session_messages(&info.name);
            let after =
                usize::try_from(since.get(&info.name).copied().unwrap_or(0)).unwrap_or(usize::MAX);
            let chars: usize = msgs
                .get(after..)
                .unwrap_or(&[])
                .iter()
                .map(Message::char_count)
                .sum();
            let tokens = estimate_tokens_from_chars(chars);
            if tokens > 0 {
                out.insert(info.name, u64::try_from(tokens).unwrap_or(u64::MAX));
            }
        }
        Ok(out)
    }

    async fn transcript_since(
        &self,
        session: &str,
        after: u64,
        max_tokens: u64,
    ) -> Result<Vec<Message>, EngineError> {
        let msgs = self.session_messages(session);
        let after = usize::try_from(after).unwrap_or(usize::MAX);
        let mut out = Vec::new();
        let mut total: u64 = 0;
        for msg in msgs.get(after..).unwrap_or(&[]) {
            let tokens =
                u64::try_from(estimate_tokens_from_chars(msg.char_count())).unwrap_or(u64::MAX);
            if !out.is_empty() && total + tokens > max_tokens {
                break;
            }
            out.push(msg.clone());
            total += tokens;
        }
        Ok(out)
    }
}

// ── Active session persistence ──────────────────────────────────────

fn session_path(sessions_dir: &Path, name: &str) -> PathBuf {
    let sanitized = sanitize_name(name);
    sessions_dir.join(format!("{sanitized}.json"))
}

/// Read the active session name from `context/active_session`.
fn read_active_session(context_dir: &Path) -> Option<String> {
    let path = context_dir.join("active_session");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist the active session name atomically.
fn persist_active_session(context_dir: &Path, name: &str) {
    let path = context_dir.join("active_session");
    let tmp = context_dir.join("active_session.tmp");
    if fs::write(&tmp, name).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, sync::Arc};

    use super::*;

    /// Build a `SummarizeFn` that returns a canned response.
    fn mock_summarize(response: &str) -> SummarizeFn {
        let response = response.to_string();
        Arc::new(move |_prompt: &str, _messages: &[Message]| {
            let response = response.clone();
            Box::pin(async move { Ok(response) })
                as Pin<Box<dyn Future<Output = Result<String, _>> + Send>>
        })
    }

    fn tiny_config() -> ContextConfig {
        ContextConfig {
            max_tokens: 100,
            budget_percent: 50,
            ..ContextConfig::default()
        }
    }

    fn temp_engine(ctx: ContextConfig) -> FlatSession {
        let dir = tempfile::tempdir().unwrap();
        temp_engine_at(&dir.keep(), ctx)
    }

    fn temp_engine_at(base: &Path, ctx: ContextConfig) -> FlatSession {
        FlatSession::new(base.join("context"), ctx).unwrap()
    }

    // ── Basic operations ────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_tool_output_is_truncated_tail_biased() {
        let ctx = ContextConfig {
            tool_output_tokens: 100,
            ..ContextConfig::default()
        };
        let mut engine = temp_engine(ctx);
        let payload = format!("HEAD{}TAIL", "x".repeat(10_000));
        engine
            .push_message(Message::Tool {
                call_id: "c1".to_string(),
                content: payload,
            })
            .await
            .unwrap();

        let stored = engine.session.messages()[0].content();
        assert!(stored.starts_with("HEAD"));
        assert!(stored.ends_with("TAIL"));
        assert!(stored.contains("tokens truncated"));
        assert!(stored.len() < 500);
    }

    #[tokio::test]
    async fn oversized_user_message_is_not_truncated() {
        let ctx = ContextConfig {
            tool_output_tokens: 100,
            ..ContextConfig::default()
        };
        let mut engine = temp_engine(ctx);
        let payload = "u".repeat(10_000);
        engine
            .push_message(Message::User {
                content: payload.clone(),
            })
            .await
            .unwrap();
        assert_eq!(engine.session.messages()[0].content(), payload);
    }

    #[test]
    fn contributes_no_tools_in_any_scope() {
        let engine = temp_engine(ContextConfig::default());
        assert!(engine.tools(ToolScope::Root).is_empty());
        assert!(engine.tools(ToolScope::SubAgent).is_empty());
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

    // ── Observed token tests ────────────────────────────────────────

    #[tokio::test]
    async fn observed_tokens_trigger_compaction_when_estimate_is_low() {
        // Budget is 50; two tiny messages estimate near zero.
        let mut engine = temp_engine(tiny_config());
        engine
            .push_message(Message::User {
                content: "a".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "b".into(),
            })
            .await
            .unwrap();

        let summarize = mock_summarize("summary");
        assert!(
            engine
                .compact_if_needed(&summarize)
                .await
                .unwrap()
                .is_none()
        );

        engine.observe_tokens(100);
        let event = engine.compact_if_needed(&summarize).await.unwrap().unwrap();
        assert_eq!(event.before, 100);
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn observation_cleared_after_compaction() {
        let mut engine = temp_engine(tiny_config());
        engine
            .push_message(Message::User {
                content: "a".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "b".into(),
            })
            .await
            .unwrap();
        engine.observe_tokens(100);

        let summarize = mock_summarize("summary");
        engine.compact_if_needed(&summarize).await.unwrap().unwrap();

        // A stale observation would re-trigger here forever.
        engine
            .push_message(Message::User {
                content: "c".into(),
            })
            .await
            .unwrap();
        assert!(
            engine
                .compact_if_needed(&summarize)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stats_report_observed_tokens_when_larger() {
        let mut engine = temp_engine(tiny_config());
        engine.observe_tokens(100);
        assert_eq!(engine.stats().token_estimate, 100);
    }

    #[tokio::test]
    async fn observation_cleared_on_clear_and_switch() {
        let mut engine = temp_engine(tiny_config());
        engine.observe_tokens(100);
        engine.clear().await.unwrap();
        assert_eq!(engine.stats().token_estimate, 0);

        engine.observe_tokens(100);
        engine.switch_session("other").await.unwrap();
        assert_eq!(engine.stats().token_estimate, 0);
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

    /// `report` reads the active session from memory and every other
    /// one from disk. A repo-bound name (`owner/repo`) is stored
    /// sanitized, so matching it against a desanitized stem failed and
    /// the session landed in the report twice.
    #[tokio::test]
    async fn report_counts_a_repo_session_once() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextConfig::default();
        let mut engine = temp_engine_at(dir.path(), ctx);

        engine.switch_session("owner/repo").await.unwrap();
        engine
            .push_message(Message::User {
                content: "only once".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();

        // One entry per session file, however many that is: switching
        // away from `general` saves it, so the count is not the point —
        // that no session is counted twice is.
        let files = fs::read_dir(dir.path().join("context/sessions"))
            .unwrap()
            .count();
        let report = engine.report().await.unwrap();
        assert!(
            report.starts_with(&format!("Tool Usage ({files} session")),
            "expected {files} sessions, active one counted twice: {report}"
        );
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

    // ── Distillation tests ──────────────────────────────────────────

    #[tokio::test]
    async fn latest_positions_reports_session_tips() {
        let mut engine = temp_engine(ContextConfig::default());
        assert!(engine.latest_positions().await.unwrap().is_empty());
        for content in ["one", "two"] {
            engine
                .push_message(Message::User {
                    content: content.into(),
                })
                .await
                .unwrap();
        }
        let tips = engine.latest_positions().await.unwrap();
        assert_eq!(tips.get("general"), Some(&2));
    }

    #[tokio::test]
    async fn pending_distill_tokens_sums_undistilled_per_session() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "a".repeat(400),
            })
            .await
            .unwrap();

        // No watermark: the whole session is pending.
        let pending = engine
            .pending_distill_tokens(&BTreeMap::new())
            .await
            .unwrap();
        assert!(pending.get("general").copied().unwrap_or(0) > 0);

        // Watermark past the only message: nothing pending.
        let mut since = BTreeMap::new();
        since.insert("general".to_string(), 1);
        let pending = engine.pending_distill_tokens(&since).await.unwrap();
        assert!(!pending.contains_key("general"));
    }

    #[tokio::test]
    async fn transcript_since_returns_span_after_watermark() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "first".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "second".into(),
            })
            .await
            .unwrap();

        let span = engine
            .transcript_since("general", 1, u64::MAX)
            .await
            .unwrap();
        assert_eq!(span.len(), 1);
        assert!(matches!(&span[0], Message::Assistant { content } if content == "second"));
    }

    #[tokio::test]
    async fn transcript_since_clamps_but_makes_progress() {
        let mut engine = temp_engine(ContextConfig::default());
        engine
            .push_message(Message::User {
                content: "a".repeat(400),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: "b".repeat(400),
            })
            .await
            .unwrap();

        // Zero budget still yields the head event.
        let span = engine.transcript_since("general", 0, 0).await.unwrap();
        assert_eq!(span.len(), 1);
    }

    // ── Report tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn report_covers_all_sessions() {
        use crate::types::{ToolCall, ToolFunction};

        let mut engine = temp_engine(ContextConfig::default());

        // Tool call in "general", saved to disk.
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
        engine.save().await.unwrap();

        // Tool call in "other", unsaved (in-memory only).
        engine.switch_session("other").await.unwrap();
        engine
            .push_message(Message::ToolCalls {
                content: String::new(),
                calls: vec![ToolCall::new(
                    "c2".into(),
                    ToolFunction {
                        name: "file_read".into(),
                        arguments: r#"{"path":"f"}"#.into(),
                    },
                )],
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Tool {
                call_id: "c2".into(),
                content: "data".into(),
            })
            .await
            .unwrap();

        let report = engine.report().await.unwrap();
        assert!(report.contains("Tool Usage (2 sessions)"));
        assert!(report.contains("exec"));
        assert!(report.contains("file_read"));
        assert!(report.contains("git status"));
    }
}
