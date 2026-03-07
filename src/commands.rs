//! Slash command definitions shared across channels.
//!
//! Each channel parses its own transport format (text lines, NDJSON,
//! Telegram messages) and maps to [`SlashCommand`]. Execution logic
//! lives here so every channel behaves identically.

use std::path::Path;
use std::str::FromStr;

use tracing::error;

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
pub fn greeting(message_count: usize) -> String {
    if message_count == 0 {
        "New session".to_string()
    } else {
        format!("Resumed session ({message_count} messages)")
    }
}

/// Execute a slash command, mutating the session in place.
///
/// Returns the result message on success, or an error message on failure.
/// Saves the session to disk when the command modifies it.
pub async fn execute<P: Provider>(
    cmd: SlashCommand,
    session: &mut Session,
    session_path: &Path,
    workspace: &Workspace,
    provider: &P,
    ctx: &ContextConfig,
) -> Result<String, String> {
    match cmd {
        SlashCommand::Compact => {
            let system_prompt = workspace.system_prompt();
            let before = context::session_tokens(session, system_prompt.len());
            match context::force_compact(session, provider).await {
                Ok(true) => {
                    let after = context::session_tokens(session, system_prompt.len());
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
            let tokens = context::session_tokens(session, system_prompt.len());
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
        assert_eq!(greeting(0), "New session");
    }

    #[test]
    fn greeting_resumed_session() {
        assert_eq!(greeting(5), "Resumed session (5 messages)");
    }
}
