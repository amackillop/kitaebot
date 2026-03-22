//! Slash command definitions shared across channels.
//!
//! Execution logic lives here so every channel behaves identically.
//! Input classification and routing lives in [`crate::dispatch`].

use std::path::Path;
use std::str::FromStr;

use tracing::error;

use crate::agent;
use crate::dispatch::Reply;
use crate::engine::{ContextEngine, SummarizeFn};
use crate::heartbeat;
use crate::provider::Provider;
use crate::session::Session;
use crate::stats;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// A recognized slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    /// Force context compaction.
    Compact,
    /// Display token usage.
    Context,
    /// Trigger a one-shot heartbeat cycle.
    Heartbeat,
    /// Clear session and start fresh.
    New,
    /// Show session tool usage statistics.
    Stats,
}

/// The input starts with `/` but doesn't match any known command.
#[derive(Debug, PartialEq)]
pub struct UnknownCommand;

impl FromStr for SlashCommand {
    type Err = UnknownCommand;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "/compact" => Ok(Self::Compact),
            "/context" => Ok(Self::Context),
            "/heartbeat" => Ok(Self::Heartbeat),
            "/new" => Ok(Self::New),
            "/stats" => Ok(Self::Stats),
            _ => Err(UnknownCommand),
        }
    }
}

/// Format the session greeting shown on connect/startup.
///
/// Loads the session from disk to count messages. Returns "New session"
/// if the file is missing or empty.
pub fn greeting(session_path: &Path) -> String {
    let count = Session::load(session_path).map_or(0, |s| s.messages().len());
    if count == 0 {
        "New session".to_string()
    } else {
        format!("Resumed session ({count} messages)")
    }
}

/// Execute a slash command.
///
/// Called by the agent actor. `/heartbeat` calls `agent::process_message`
/// directly rather than going through the handle (which would deadlock).
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cmd: SlashCommand,
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
) -> Result<Reply, String> {
    match cmd {
        SlashCommand::Compact => match engine.force_compact(summarize).await {
            Ok(event) => {
                if event.before == 0 && event.after == 0 {
                    Ok(Reply::text("Nothing to compact.".into()))
                } else {
                    if let Err(e) = engine.save().await {
                        error!("Failed to save session: {e}");
                    }
                    Ok(Reply::text(format!(
                        "Compacted: {} -> {} tokens",
                        event.before, event.after,
                    )))
                }
            }
            Err(e) => Err(format!("Compaction failed: {e}")),
        },
        SlashCommand::Context => {
            let stats = engine.stats();
            let pct = if stats.budget > 0 {
                (stats.token_estimate / stats.budget) * 100
            } else {
                0
            };
            Ok(Reply::text(format!(
                "Context: {} / {} tokens ({pct}%)\n\
                 Messages: {}\n\
                 Session: {}",
                stats.token_estimate,
                stats.budget,
                stats.message_count,
                engine.active_session(),
            )))
        }
        SlashCommand::Heartbeat => {
            use tokio_util::sync::CancellationToken;

            match heartbeat::prepare(workspace) {
                Ok(heartbeat::Prepared::Ready(prompt)) => {
                    let cancel = CancellationToken::new();
                    match agent::process_message(
                        engine,
                        summarize,
                        workspace,
                        &prompt,
                        provider,
                        tools,
                        max_iterations,
                        None,
                        &cancel,
                    )
                    .await
                    {
                        Ok(response) => {
                            if let Err(e) = heartbeat::finish(workspace, &response) {
                                error!("Failed to write heartbeat history: {e}");
                            }
                            Ok(Reply::text(response))
                        }
                        Err(e) => Err(format!("Heartbeat failed: {e}")),
                    }
                }
                Ok(heartbeat::Prepared::Skipped(reason)) => {
                    Ok(Reply::text(format!("Skipped: {reason}")))
                }
                Err(e) => Err(format!("Heartbeat failed: {e}")),
            }
        }
        SlashCommand::New => {
            engine.clear().await.map_err(|e| e.to_string())?;
            if let Err(e) = engine.save().await {
                error!("Failed to save session: {e}");
            }
            Ok(Reply::text("Session cleared.".into()))
        }
        SlashCommand::Stats => Ok(Reply::pre(stats::run(workspace.path()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_commands() {
        assert_eq!("/compact".parse(), Ok(SlashCommand::Compact));
        assert_eq!("/context".parse(), Ok(SlashCommand::Context));
        assert_eq!("/heartbeat".parse(), Ok(SlashCommand::Heartbeat));
        assert_eq!("/new".parse(), Ok(SlashCommand::New));
        assert_eq!("/stats".parse(), Ok(SlashCommand::Stats));
    }

    #[test]
    fn parse_unknown_command() {
        assert_eq!("/adsjhfbakj".parse::<SlashCommand>(), Err(UnknownCommand));
    }

    #[test]
    fn greeting_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert_eq!(greeting(&path), "New session");
    }

    #[test]
    fn greeting_resumed_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        let mut session = Session::new();
        for i in 0..5 {
            session.add_message(crate::types::Message::User {
                content: format!("msg {i}"),
            });
        }
        session.save(&path).unwrap();

        assert_eq!(greeting(&path), "Resumed session (5 messages)");
    }
}
