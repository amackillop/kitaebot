//! Slash command definitions shared across channels.
//!
//! Each channel parses its own transport format (text lines, NDJSON,
//! Telegram messages) and maps to [`SlashCommand`]. Execution logic
//! lives here so every channel behaves identically.

use std::path::Path;

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
    /// Clear session and start fresh.
    NewSession,
    /// Display token usage.
    Context,
    /// Force context compaction.
    Compact,
    /// Show session tool usage statistics.
    Stats,
}

impl SlashCommand {
    /// Parse a command name (without the leading `/`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "new" => Some(Self::NewSession),
            "context" => Some(Self::Context),
            "compact" => Some(Self::Compact),
            "stats" => Some(Self::Stats),
            _ => None,
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
        SlashCommand::NewSession => {
            session.clear();
            if let Err(e) = session.save(session_path) {
                error!("Failed to save session: {e}");
            }
            Ok("Session cleared.".into())
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
        assert_eq!(SlashCommand::parse("new"), Some(SlashCommand::NewSession));
        assert_eq!(SlashCommand::parse("context"), Some(SlashCommand::Context));
        assert_eq!(SlashCommand::parse("compact"), Some(SlashCommand::Compact));
        assert_eq!(SlashCommand::parse("stats"), Some(SlashCommand::Stats));
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(SlashCommand::parse("help"), None);
        assert_eq!(SlashCommand::parse("exit"), None);
        assert_eq!(SlashCommand::parse(""), None);
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
