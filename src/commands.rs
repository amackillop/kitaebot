//! Slash command definitions shared across channels.
//!
//! Execution logic lives here so every channel behaves identically.
//! Input classification and routing lives in [`crate::dispatch`].

use std::fmt::Write as _;
use std::str::FromStr;

use tracing::error;

use crate::agent;
use crate::dispatch::Reply;
use crate::engine::{ContextEngine, SummarizeFn};
use crate::heartbeat;
use crate::memory::distill::{self, Distiller};
use crate::provider::Provider;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// A recognized slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Force context compaction.
    Compact,
    /// Display token usage.
    Context,
    /// Trigger a one-shot heartbeat cycle.
    Heartbeat,
    /// Clear session and start fresh.
    New,
    /// List sessions or switch to a named one.
    Project { name: Option<String> },
    /// Show session tool usage statistics.
    Stats,
}

/// The input starts with `/` but doesn't match any known command.
#[derive(Debug, PartialEq)]
pub struct UnknownCommand;

impl FromStr for SlashCommand {
    type Err = UnknownCommand;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        // Tokenize: at most two whitespace-separated parts. More is an error.
        let mut parts = input.split_whitespace();
        let head = parts.next().ok_or(UnknownCommand)?;
        let arg = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(UnknownCommand);
        }

        match (head, arg) {
            ("/compact", "") => Ok(Self::Compact),
            ("/context", "") => Ok(Self::Context),
            ("/heartbeat", "") => Ok(Self::Heartbeat),
            ("/new", "") => Ok(Self::New),
            ("/stats", "") => Ok(Self::Stats),
            ("/project", "") => Ok(Self::Project { name: None }),
            ("/project", name) => Ok(Self::Project {
                name: Some(name.to_string()),
            }),
            _ => Err(UnknownCommand),
        }
    }
}

/// Execute a slash command.
///
/// Called by the agent actor. `/heartbeat` calls `agent::process_message`
/// directly rather than going through the handle (which would deadlock).
/// The heartbeat and memory providers are consumed only by the
/// `/heartbeat` arm: the review turn may run on a cheaper model than
/// root turns, and distillation runs on its own memory model.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cmd: SlashCommand,
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    heartbeat_provider: &impl Provider,
    memory_provider: &impl Provider,
    tools: &Tools,
    distiller: &Distiller,
    max_iterations: usize,
    memory_index_cap: usize,
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
                stats.token_estimate * 100 / stats.budget
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
            // The task-review turn. May be skipped when there are no
            // standing tasks; distillation below runs regardless.
            let review = match heartbeat::prepare(workspace) {
                Ok(heartbeat::Prepared::Ready(prompt)) => {
                    let output = agent::process_message(
                        engine,
                        summarize,
                        workspace,
                        &prompt,
                        heartbeat_provider,
                        tools,
                        max_iterations,
                        memory_index_cap,
                        &crate::tools::ToolCtx::default(),
                    )
                    .await
                    .map_err(|e| format!("Heartbeat failed: {e}"))?;
                    let response = output.into_text();
                    if let Err(e) = heartbeat::finish(workspace, &response) {
                        error!("Failed to write heartbeat history: {e}");
                    }
                    response
                }
                Ok(heartbeat::Prepared::Skipped(reason)) => format!("Skipped: {reason}"),
                Err(e) => return Err(format!("Heartbeat failed: {e}")),
            };

            // Distillation is independent of the review: it folds
            // accumulated session history into memory whenever the
            // mechanical gate opens, even with no review tasks. A
            // failure is logged, not fatal — the review reply stands.
            let mut state = distill::DistillState::load(&workspace.distillation_state_path());
            match distill::run(
                engine,
                distiller,
                memory_provider,
                summarize,
                workspace,
                &mut state,
            )
            .await
            {
                Ok(Some(summary)) => {
                    if let Err(e) = heartbeat::finish(workspace, &format!("Distilled: {summary}")) {
                        error!("Failed to write distillation history: {e}");
                    }
                    Ok(Reply::text(format!("{review}\n\nDistilled: {summary}")))
                }
                Ok(None) => Ok(Reply::text(review)),
                Err(e) => {
                    error!("Distillation failed: {e}");
                    Ok(Reply::text(review))
                }
            }
        }
        SlashCommand::New => {
            engine.clear().await.map_err(|e| e.to_string())?;
            if let Err(e) = engine.save().await {
                error!("Failed to save session: {e}");
            }
            Ok(Reply::text("Session cleared.".into()))
        }
        SlashCommand::Project { name } => project(engine, name).await,
        SlashCommand::Stats => engine
            .report()
            .await
            .map(Reply::pre)
            .map_err(|e| e.to_string()),
    }
}

/// Dispatch `/project` with or without a name argument.
async fn project(engine: &mut impl ContextEngine, name: Option<String>) -> Result<Reply, String> {
    match name {
        None => list_projects(engine).await,
        Some(raw) => switch_project(engine, &raw).await,
    }
}

async fn list_projects(engine: &mut impl ContextEngine) -> Result<Reply, String> {
    let sessions = engine.list_sessions().await.map_err(|e| e.to_string())?;
    let active = engine.active_session();
    let mut out = String::new();
    for s in &sessions {
        let marker = if s.name == active { "* " } else { "  " };
        let _ = writeln!(
            out,
            "{marker}{} ({} messages, ~{} tokens)",
            s.name, s.message_count, s.estimated_tokens,
        );
    }
    if out.is_empty() {
        out.push_str("No sessions.\n");
    }
    Ok(Reply::pre(out))
}

async fn switch_project(engine: &mut impl ContextEngine, name: &str) -> Result<Reply, String> {
    // Filename sanitization (`/`, `..`, null bytes) is the engine's job.
    engine
        .switch_session(name)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = engine.save().await {
        error!("Failed to save session: {e}");
    }
    Ok(Reply::text(format!(
        "Switched to '{}' ({} messages)",
        engine.active_session(),
        engine.stats().message_count,
    )))
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
    fn parse_project_no_arg() {
        assert_eq!("/project".parse(), Ok(SlashCommand::Project { name: None }));
    }

    #[test]
    fn parse_project_with_name() {
        assert_eq!(
            "/project foo".parse(),
            Ok(SlashCommand::Project {
                name: Some("foo".into())
            }),
        );
    }

    #[test]
    fn parse_project_rejects_multi_token_name() {
        assert_eq!(
            "/project foo bar".parse::<SlashCommand>(),
            Err(UnknownCommand),
        );
    }

    #[test]
    fn parse_zero_arg_rejects_extras() {
        assert_eq!("/new junk".parse::<SlashCommand>(), Err(UnknownCommand));
        assert_eq!("/stats x".parse::<SlashCommand>(), Err(UnknownCommand));
    }
}
