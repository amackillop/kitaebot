#![allow(dead_code)]

//! Agent actor run loop.
//!
//! The [`Agent`] struct owns the session, provider, tools, and config.
//! It processes one [`Envelope`] at a time in a sequential loop, which
//! eliminates the need for session locking or `Arc<Mutex<Session>>`.
//!
//! Spawned by [`AgentHandle::spawn`](super::AgentHandle::spawn).

use std::sync::Arc;

use crate::commands;
use crate::config::ContextConfig;
use crate::dispatch::{Input, Reply};
use crate::provider::Provider;
use crate::tools::Tools;
use crate::workspace::Workspace;
use tokio::sync::mpsc;

use super::envelope::Envelope;

/// The actor that processes envelopes sequentially.
///
/// Owns all dependencies so the run loop has no borrows and is `'static`.
pub(super) struct Agent<P: Provider> {
    rx: mpsc::Receiver<Envelope>,
    workspace: Arc<Workspace>,
    provider: Arc<P>,
    tools: Arc<Tools>,
    max_iterations: usize,
    ctx: ContextConfig,
}

impl<P: Provider + 'static> Agent<P> {
    pub fn new(
        rx: mpsc::Receiver<Envelope>,
        workspace: Arc<Workspace>,
        provider: Arc<P>,
        tools: Arc<Tools>,
        max_iterations: usize,
        ctx: ContextConfig,
    ) -> Self {
        Self {
            rx,
            workspace,
            provider,
            tools,
            max_iterations,
            ctx,
        }
    }

    /// Consume envelopes until all handles are dropped.
    pub async fn run(mut self) {
        let session_path = self.workspace.session_path();
        while let Some(envelope) = self.rx.recv().await {
            let result = self.handle(&envelope, &session_path).await;
            let _ = envelope.reply_tx.send(result);
        }
    }

    async fn handle(
        &self,
        envelope: &Envelope,
        session_path: &std::path::Path,
    ) -> Result<Reply, String> {
        match Input::parse(&envelope.input) {
            Ok(Input::Command(cmd)) => {
                commands::execute(
                    cmd,
                    session_path,
                    &self.workspace,
                    &*self.provider,
                    &self.tools,
                    self.max_iterations,
                    self.ctx,
                )
                .await
            }
            Ok(Input::Message(text)) => {
                let tagged = format!("[{}]: {text}", envelope.source);
                super::process_message(
                    session_path,
                    &self.workspace,
                    &tagged,
                    &*self.provider,
                    &self.tools,
                    self.max_iterations,
                    self.ctx,
                    envelope.activity_tx.as_ref(),
                    &envelope.cancel,
                )
                .await
                .map(Reply::text)
                .map_err(|e| e.to_string())
            }
            Err(_) => Err(format!("Unknown command: {}", envelope.input)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHandle;
    use crate::agent::envelope::ChannelSource;
    use crate::provider::MockProvider;
    use crate::types::Response;
    use tokio_util::sync::CancellationToken;

    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, Arc::new(ws))
    }

    #[tokio::test]
    async fn text_roundtrip() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "hello back".into(),
        ))]));
        let tools = Arc::new(Tools::default());

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hello".into(),
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
        let tools = Arc::new(Tools::default());

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/new".into(),
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
        let tools = Arc::new(Tools::default());

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/bogus".into(),
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
        let tools = Arc::new(Tools::default());

        let cancel = CancellationToken::new();
        cancel.cancel();

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);
        let result = handle
            .send_message(ChannelSource::Socket, "hi".into(), None, cancel)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sequential_messages_share_session() {
        let (_dir, ws) = workspace();
        // Two responses for two messages.
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));
        let tools = Arc::new(Tools::default());

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);

        let r1 = handle
            .send_message(
                ChannelSource::Telegram,
                "msg1".into(),
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
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r2.unwrap().content, "second");
    }

    #[tokio::test]
    async fn drop_handle_shuts_down_actor() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));
        let tools = Arc::new(Tools::default());

        let handle = AgentHandle::spawn(ws, provider, tools, 1, CTX);
        drop(handle);
        // If the actor panicked or hung, the test runtime would catch it.
        // Reaching here means clean shutdown.
    }
}
