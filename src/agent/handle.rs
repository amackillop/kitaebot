#![allow(dead_code)]

//! Typed handle for communicating with the agent actor.
//!
//! Follows [Ryhl's actor pattern](https://ryhl.io/blog/actors-with-tokio/):
//! each public method on the handle maps to one message type. Callers never
//! see [`Envelope`] — they call methods and `await` the reply.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::activity::Activity;
use crate::config::ContextConfig;
use crate::dispatch::Reply;
use crate::provider::Provider;
use crate::tools::Tools;
use crate::workspace::Workspace;

use super::actor::Agent;
use super::envelope::{ChannelSource, Envelope};

/// Cloneable handle to the agent actor.
///
/// Channels hold one clone each. When every clone is dropped the actor's
/// receiver returns `None` and it shuts down.
#[derive(Clone)]
pub struct AgentHandle {
    tx: mpsc::Sender<Envelope>,
}

impl AgentHandle {
    /// Spawn the agent actor and return a handle to it.
    ///
    /// The actor task runs until all handles are dropped.
    pub fn spawn<P: Provider + 'static>(
        workspace: Arc<Workspace>,
        provider: Arc<P>,
        tools: Arc<Tools>,
        max_iterations: usize,
        ctx: ContextConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let actor = Agent::new(rx, workspace, provider, tools, max_iterations, ctx);
        tokio::spawn(actor.run());
        Self { tx }
    }

    /// Create a handle wrapping an existing sender.
    #[cfg(test)]
    pub(super) fn new(tx: mpsc::Sender<Envelope>) -> Self {
        Self { tx }
    }

    /// Send a message to the agent and await the reply.
    pub async fn send_message(
        &self,
        source: ChannelSource,
        input: String,
        activity_tx: Option<mpsc::Sender<Activity>>,
        cancel: CancellationToken,
    ) -> Result<Reply, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let envelope = Envelope {
            source,
            input,
            reply_tx,
            activity_tx,
            cancel,
        };
        let _ = self.tx.send(envelope).await;
        reply_rx
            .await
            .unwrap_or_else(|_| Err("Agent shut down".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_shutdown_returns_error() {
        let (tx, rx) = mpsc::channel(1);
        let handle = AgentHandle::new(tx);

        // Drop the receiver so the actor is "dead".
        drop(rx);

        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hello".into(),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Agent shut down");
    }

    #[tokio::test]
    async fn roundtrip_through_channel() {
        let (tx, mut rx) = mpsc::channel(1);
        let handle = AgentHandle::new(tx);

        let reply_fut = handle.send_message(
            ChannelSource::Telegram,
            "ping".into(),
            None,
            CancellationToken::new(),
        );

        // Simulate actor: recv envelope and reply.
        let actor_fut = async {
            let envelope = rx.recv().await.unwrap();
            assert_eq!(envelope.input, "ping");
            assert!(matches!(envelope.source, ChannelSource::Telegram));
            let _ = envelope.reply_tx.send(Ok(Reply::text("pong".into())));
        };

        let (result, ()) = tokio::join!(reply_fut, actor_fut);
        assert_eq!(result.unwrap().content, "pong");
    }
}
