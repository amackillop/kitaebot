//! Actor message protocol.
//!
//! [`Envelope`] is the single message type the agent actor receives.
//! Callers never construct envelopes directly — they use
//! [`AgentHandle::send_message`](super::AgentHandle) instead.

use std::fmt;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::activity::Activity;
use crate::dispatch::Reply;

/// Which channel originated a message.
///
/// Prefixed onto messages in the unified session so the agent (and the
/// human reviewing logs) can tell where input came from.
#[derive(Debug, Clone)]
pub enum ChannelSource {
    Heartbeat,
    GitHub {
        pr_number: u32,
        #[allow(dead_code)] // Used in followup commit (GitHub session routing).
        repo: String,
    },
    Socket,
    Telegram,
}

impl fmt::Display for ChannelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Heartbeat => write!(f, "Heartbeat"),
            Self::GitHub { pr_number, .. } => write!(f, "GitHub PR #{pr_number}"),
            Self::Socket => write!(f, "Socket"),
            Self::Telegram => write!(f, "Telegram"),
        }
    }
}

/// Internal message sent from [`AgentHandle`](super::AgentHandle) to the actor.
pub(super) struct Envelope {
    pub source: ChannelSource,
    pub input: String,
    /// Target session override. `None` means use the active session.
    #[allow(dead_code)] // Used in followup commit (actor ephemeral switching).
    pub session_hint: Option<String>,
    pub reply_tx: oneshot::Sender<Result<Reply, String>>,
    pub activity_tx: Option<mpsc::Sender<Activity>>,
    pub cancel: CancellationToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_heartbeat() {
        assert_eq!(ChannelSource::Heartbeat.to_string(), "Heartbeat");
    }

    #[test]
    fn display_github() {
        let src = ChannelSource::GitHub {
            pr_number: 42,
            repo: "owner/repo".into(),
        };
        assert_eq!(src.to_string(), "GitHub PR #42");
    }

    #[test]
    fn display_socket() {
        assert_eq!(ChannelSource::Socket.to_string(), "Socket");
    }

    #[test]
    fn display_telegram() {
        assert_eq!(ChannelSource::Telegram.to_string(), "Telegram");
    }
}
