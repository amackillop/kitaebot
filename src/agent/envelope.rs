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
    /// The duty scheduler (spec 24).
    Duty,
    GitHub {
        pr_number: u32,
        #[allow(dead_code)] // Routed via session_hint, not consumed by actor.
        repo: String,
    },
    Linear {
        issue: String,
    },
    Socket,
    Telegram,
}

impl ChannelSource {
    /// Whether a human is watching the reply. Failed unattended turns
    /// are pushed to the user via the notifier.
    pub fn is_attended(&self) -> bool {
        matches!(self, Self::Socket | Self::Telegram)
    }
}

impl fmt::Display for ChannelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duty => write!(f, "Duty"),
            Self::GitHub { pr_number, .. } => write!(f, "GitHub PR #{pr_number}"),
            Self::Linear { issue } => write!(f, "Linear {issue}"),
            Self::Socket => write!(f, "Socket"),
            Self::Telegram => write!(f, "Telegram"),
        }
    }
}

/// Internal message sent from [`AgentHandle`](super::AgentHandle) to the actor.
///
/// Two operations are multiplexed on the same channel: full input turns
/// (commands or free text) and lightweight greeting requests for the
/// socket greeting.
pub(super) enum Envelope {
    Input(InputEnvelope),
    /// Request the actor to format a session greeting and reply with it.
    Greeting(oneshot::Sender<String>),
}

/// Payload for a normal input turn.
pub(super) struct InputEnvelope {
    pub source: ChannelSource,
    pub input: String,
    /// Target session override. `None` means use the active session.
    pub session_hint: Option<String>,
    pub reply_tx: oneshot::Sender<Result<Reply, String>>,
    pub activity_tx: Option<mpsc::Sender<Activity>>,
    pub cancel: CancellationToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_socket_and_telegram_are_attended() {
        assert!(ChannelSource::Socket.is_attended());
        assert!(ChannelSource::Telegram.is_attended());
        assert!(!ChannelSource::Duty.is_attended());
        assert!(
            !ChannelSource::GitHub {
                pr_number: 1,
                repo: "owner/repo".into(),
            }
            .is_attended()
        );
        assert!(
            !ChannelSource::Linear {
                issue: "MDK-1".into(),
            }
            .is_attended()
        );
    }

    #[test]
    fn display_duty() {
        assert_eq!(ChannelSource::Duty.to_string(), "Duty");
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
    fn display_linear() {
        let src = ChannelSource::Linear {
            issue: "MDK-123".into(),
        };
        assert_eq!(src.to_string(), "Linear MDK-123");
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
