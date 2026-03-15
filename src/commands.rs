//! Slash command definitions shared across channels.
//!
//! Execution logic lives here so every channel behaves identically.
//! Input classification and routing lives in [`crate::dispatch`].

use std::path::Path;
use std::str::FromStr;

use tracing::error;

use crate::agent;
use crate::config::ContextConfig;
use crate::context;
use crate::dispatch::Reply;
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
/// Called by the agent actor. `session_path` is the unified session
/// managed by the actor. `/heartbeat` calls `agent::process_message`
/// directly rather than going through the handle (which would deadlock).
#[allow(clippy::too_many_arguments)]
pub async fn execute<P: Provider>(
    cmd: SlashCommand,
    session_path: &Path,
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    ctx: ContextConfig,
) -> Result<Reply, String> {
    let mut session =
        Session::load(session_path).map_err(|e| format!("Session load error: {e}"))?;

    match cmd {
        SlashCommand::Compact => {
            let system_prompt = workspace.system_prompt();
            let before = context::session_tokens(&session, system_prompt.len());
            match context::force_compact(&mut session, provider).await {
                Ok(true) => {
                    let after = context::session_tokens(&session, system_prompt.len());
                    if let Err(e) = session.save(session_path) {
                        error!("Failed to save session: {e}");
                    }
                    Ok(Reply::text(format!(
                        "Compacted: {before} -> {after} tokens"
                    )))
                }
                Ok(false) => Ok(Reply::text("Nothing to compact.".into())),
                Err(e) => Err(format!("Compaction failed: {e}")),
            }
        }
        SlashCommand::Context => {
            let system_prompt = workspace.system_prompt();
            let tokens = context::session_tokens(&session, system_prompt.len());
            let budget = context::budget(ctx);
            let pct = if budget > 0 {
                (tokens / budget) * 100
            } else {
                0
            };
            Ok(Reply::text(format!(
                "Context: {tokens} / {budget} tokens ({pct}%)\n\
                 Messages: {}\n\
                 Budget: {}% of {}",
                session.len(),
                ctx.budget_percent,
                ctx.max_tokens,
            )))
        }
        SlashCommand::Heartbeat => {
            use tokio_util::sync::CancellationToken;

            match heartbeat::prepare(workspace) {
                Ok(heartbeat::Prepared::Ready(ready)) => {
                    let cancel = CancellationToken::new();
                    match agent::process_message(
                        session_path,
                        workspace,
                        &ready.prompt,
                        provider,
                        tools,
                        max_iterations,
                        ctx,
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
            session.clear();
            if let Err(e) = session.save(session_path) {
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
