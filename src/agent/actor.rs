//! Agent actor run loop.
//!
//! The [`Agent`] struct owns the engine, provider, tools, and config.
//! It processes one [`Envelope`] at a time in a sequential loop, which
//! eliminates the need for session locking or `Arc<Mutex<Session>>`.
//!
//! Spawned by [`AgentHandle::spawn`](super::AgentHandle::spawn).

use std::sync::Arc;

use tracing::error;

use crate::commands;
use crate::dispatch::{Input, Reply};
use crate::engine::{ContextEngine, SummarizeFn};
use crate::provider::Provider;
use crate::tools::Tools;
use crate::workspace::Workspace;
use tokio::sync::mpsc;

use super::envelope::Envelope;

/// The actor that processes envelopes sequentially.
///
/// Owns all dependencies so the run loop has no borrows and is `'static`.
pub(super) struct Agent<P: Provider, E: ContextEngine> {
    rx: mpsc::Receiver<Envelope>,
    workspace: Arc<Workspace>,
    provider: Arc<P>,
    tools: Arc<Tools>,
    max_iterations: usize,
    engine: E,
    summarize: SummarizeFn,
}

impl<P: Provider + 'static, E: ContextEngine + 'static> Agent<P, E> {
    pub fn new(
        rx: mpsc::Receiver<Envelope>,
        workspace: Arc<Workspace>,
        provider: Arc<P>,
        tools: Arc<Tools>,
        max_iterations: usize,
        engine: E,
        summarize: SummarizeFn,
    ) -> Self {
        Self {
            rx,
            workspace,
            provider,
            tools,
            max_iterations,
            engine,
            summarize,
        }
    }

    /// Consume envelopes until all handles are dropped.
    pub async fn run(mut self) {
        while let Some(envelope) = self.rx.recv().await {
            let result = self.handle(&envelope).await;
            let _ = envelope.reply_tx.send(result);
        }
    }

    async fn handle(&mut self, envelope: &Envelope) -> Result<Reply, String> {
        match Input::parse(&envelope.input) {
            Ok(Input::Command(cmd)) => {
                commands::execute(
                    cmd,
                    &mut self.engine,
                    &self.summarize,
                    &self.workspace,
                    &*self.provider,
                    &self.tools,
                    self.max_iterations,
                )
                .await
            }
            Ok(Input::Message(text)) => self.handle_message(envelope, text).await,
            Err(_) => Err(format!("Unknown command: {}", envelope.input)),
        }
    }

    /// Process a free-text message, optionally switching sessions for the turn.
    ///
    /// If `envelope.session_hint` differs from the active session, switch to it
    /// before processing and restore the original active session afterward.
    /// This is how GitHub PRs get routed to per-repo sessions while keeping
    /// Telegram/Socket on whatever the user's `/project` selection was.
    async fn handle_message(&mut self, envelope: &Envelope, text: &str) -> Result<Reply, String> {
        let original = self.engine.active_session().to_string();
        let target = envelope.session_hint.as_deref().unwrap_or(&original);
        let switched = target != original;

        if switched {
            // switch_session saves the current session before loading the target.
            if let Err(e) = self.engine.switch_session(target).await {
                return Err(format!("Failed to switch session: {e}"));
            }
        }

        let tagged = format!("[{}]: {text}", envelope.source);
        let result = super::process_message(
            &mut self.engine,
            &self.summarize,
            &self.workspace,
            &tagged,
            &*self.provider,
            &self.tools,
            self.max_iterations,
            envelope.activity_tx.as_ref(),
            &envelope.cancel,
        )
        .await
        .map(Reply::text)
        .map_err(|e| e.to_string());

        if switched {
            // Restore. switch_session saves the target before loading original.
            if let Err(e) = self.engine.switch_session(&original).await {
                error!("Failed to restore active session '{original}': {e}");
            }
        } else if let Err(e) = self.engine.save().await {
            error!("Failed to save session: {e}");
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHandle;
    use crate::agent::envelope::ChannelSource;
    use crate::config::ContextConfig;
    use crate::engine::flat::FlatSession;
    use crate::engine::make_summarize_fn;
    use crate::provider::MockProvider;
    use crate::types::Response;
    use tokio_util::sync::CancellationToken;

    fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, Arc::new(ws))
    }

    fn spawn_agent(ws: Arc<Workspace>, provider: Arc<MockProvider>) -> AgentHandle {
        let tools = Arc::new(Tools::default());
        let sessions_dir = ws.path().join("sessions");
        let memory_dir = ws.path().join("memory");
        let engine = FlatSession::new(sessions_dir, memory_dir, ContextConfig::default()).unwrap();
        let summarize = make_summarize_fn(provider.clone());
        AgentHandle::spawn(ws, provider, tools, 1, engine, summarize)
    }

    #[tokio::test]
    async fn text_roundtrip() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "hello back".into(),
        ))]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hello".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(result.unwrap().content, "hello back");
    }

    #[tokio::test]
    async fn slash_new_clears_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/new".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(result.unwrap().content, "Session cleared.");
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/bogus".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown command"));
    }

    #[tokio::test]
    async fn cancelled_token_returns_error() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let cancel = CancellationToken::new();
        cancel.cancel();

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(ChannelSource::Socket, "hi".into(), None, None, cancel)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sequential_messages_share_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));

        let handle = spawn_agent(ws, provider);

        let r1 = handle
            .send_message(
                ChannelSource::Telegram,
                "msg1".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r1.unwrap().content, "first");

        let r2 = handle
            .send_message(
                ChannelSource::Telegram,
                "msg2".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r2.unwrap().content, "second");
    }

    #[tokio::test]
    async fn session_hint_routes_to_named_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));

        let handle = spawn_agent(ws.clone(), provider);

        // Default active session is "general". Send to "owner/repo" via hint.
        let r1 = handle
            .send_message(
                ChannelSource::GitHub {
                    pr_number: 1,
                    repo: "owner/repo".into(),
                },
                "github msg".into(),
                Some("owner/repo".into()),
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r1.unwrap().content, "first");

        // The next message has no hint -- should land in "general", not "owner/repo".
        let r2 = handle
            .send_message(
                ChannelSource::Socket,
                "socket msg".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r2.unwrap().content, "second");

        // Verify on disk: each session has exactly one user message.
        let sessions = ws.path().join("sessions");
        let general = std::fs::read_to_string(sessions.join("general.json")).unwrap();
        let github = std::fs::read_to_string(sessions.join("owner--repo.json")).unwrap();
        assert!(general.contains("socket msg"));
        assert!(!general.contains("github msg"));
        assert!(github.contains("github msg"));
        assert!(!github.contains("socket msg"));
    }

    #[tokio::test]
    async fn drop_handle_shuts_down_actor() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        drop(handle);
    }
}
