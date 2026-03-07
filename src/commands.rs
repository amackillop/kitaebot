//! Slash command definitions and input dispatch shared across channels.
//!
//! [`dispatch`] is the single entry point for all user input: it parses
//! the text, routes to either [`execute`] or [`agent::process_message`],
//! and returns a uniform `Result<String, String>`. Channels only need to
//! handle the result in their own transport format.

use std::path::Path;
use std::str::FromStr;

use tracing::error;

use crate::agent::{self, TurnConfig};
use crate::config::ContextConfig;
use crate::context;
use crate::provider::Provider;
use crate::session::Session;
use crate::stats;
use crate::workspace::Workspace;

/// A recognized slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    /// Force context compaction.
    Compact,
    /// Display token usage.
    Context,
    /// Clear session and start fresh.
    New,
    /// Show session tool usage statistics.
    Stats,
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    MustStartWithSlash,
    UnknownCommand,
}

impl FromStr for SlashCommand {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if !input.starts_with('/') {
            return Err(ParseError::MustStartWithSlash);
        }
        match input {
            "/compact" => Ok(Self::Compact),
            "/context" => Ok(Self::Context),
            "/new" => Ok(Self::New),
            "/stats" => Ok(Self::Stats),
            _ => Err(ParseError::UnknownCommand),
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

/// Dispatch user input: parse as slash command or forward to the agent.
///
/// This is the single entry point for all channel input. Returns
/// `Ok(response)` on success or `Err(message)` on failure, both as
/// displayable strings.
pub async fn dispatch<P: Provider>(
    input: &str,
    session_path: &Path,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) -> Result<String, String> {
    match input.parse() {
        Ok(cmd) => {
            execute(
                cmd,
                session_path,
                workspace,
                config.provider,
                config.context,
            )
            .await
        }
        Err(ParseError::MustStartWithSlash) => {
            agent::process_message(session_path, workspace, input, config)
                .await
                .map_err(|e| e.to_string())
        }
        Err(ParseError::UnknownCommand) => Err(format!("Unknown command: {input}")),
    }
}

/// Execute a slash command.
///
/// Loads the session from disk, runs the command, and saves when the
/// command modifies it. Returns the result message on success, or an
/// error message on failure.
pub async fn execute<P: Provider>(
    cmd: SlashCommand,
    session_path: &Path,
    workspace: &Workspace,
    provider: &P,
    ctx: &ContextConfig,
) -> Result<String, String> {
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
                    Ok(format!("Compacted: {before} -> {after} tokens"))
                }
                Ok(false) => Ok("Nothing to compact.".into()),
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
            Ok(format!(
                "Context: {tokens} / {budget} tokens ({pct}%)\n\
                 Messages: {}\n\
                 Budget: {}% of {}",
                session.len(),
                ctx.budget_percent,
                ctx.max_tokens,
            ))
        }
        SlashCommand::New => {
            session.clear();
            if let Err(e) = session.save(session_path) {
                error!("Failed to save session: {e}");
            }
            Ok("Session cleared.".into())
        }
        SlashCommand::Stats => {
            stats::run(workspace.path());
            Ok("Stats printed to logs.".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_commands() {
        assert_eq!("/new".parse(), Ok(SlashCommand::New));
        assert_eq!("/context".parse(), Ok(SlashCommand::Context));
        assert_eq!("/compact".parse(), Ok(SlashCommand::Compact));
        assert_eq!("/stats".parse(), Ok(SlashCommand::Stats));
    }

    #[test]
    fn parse_unknown_command() {
        assert_eq!(
            "/adsjhfbakj".parse::<SlashCommand>(),
            Err(ParseError::UnknownCommand)
        );
    }

    #[test]
    fn parse_missing_slash() {
        assert_eq!(
            "missingslash".parse::<SlashCommand>(),
            Err(ParseError::MustStartWithSlash)
        );
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
