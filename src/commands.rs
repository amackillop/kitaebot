//! Slash command definitions shared across channels.
//!
//! Execution logic lives here so every channel behaves identically.
//! Input classification and routing lives in [`crate::dispatch`].

use std::fmt::Write as _;
use std::str::FromStr;

use tracing::error;

use crate::agent;
use crate::dispatch::Reply;
use crate::engine::names::sanitize_name;
use crate::engine::{ContextEngine, SessionInfo, SummarizeFn};
use crate::heartbeat;
use crate::memory::distill::{self, Distiller};
use crate::provider::Provider;
use crate::review::ReviewLedger;
use crate::tools::Tools;
use crate::usage::{self, TurnRecord, UsageLedger};
use crate::workspace::Workspace;

/// A recognized slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Force context compaction.
    Compact,
    /// Display token usage.
    Context,
    /// Force a memory distillation pass, bypassing the token gate.
    Distill,
    /// Run one named duty, gate respected (sent by the scheduler).
    Duty { name: String },
    /// Run every duty whose gate is open, ignoring schedules.
    Duties,
    /// Clear session and start fresh.
    New,
    /// List sessions or switch to a named one.
    Project { name: Option<String> },
    /// Show session tool usage statistics.
    Stats,
    /// Show recorded turn cost, broken down by build and model.
    Usage,
    /// Show review verdicts and finding counts (spec 23).
    Findings,
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
            ("/distill", "") => Ok(Self::Distill),
            ("/duties", "") => Ok(Self::Duties),
            ("/duty", name) if !name.is_empty() => Ok(Self::Duty {
                name: name.to_string(),
            }),
            ("/new", "") => Ok(Self::New),
            ("/stats", "") => Ok(Self::Stats),
            ("/usage", "") => Ok(Self::Usage),
            ("/findings", "") => Ok(Self::Findings),
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
/// Called by the agent actor. Duty arms call `agent::process_message`
/// directly rather than going through the handle (which would deadlock).
/// The task-review and memory providers are consumed only by the duty
/// arms: the task-review turn may run on a cheaper model than root
/// turns, and distillation runs on its own memory model.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cmd: SlashCommand,
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    task_review_provider: &impl Provider,
    memory_provider: &impl Provider,
    tools: &Tools,
    distiller: &Distiller,
    max_iterations: usize,
    memory_index_cap: usize,
    usage_ledger: Option<&UsageLedger>,
    review_ledger: Option<&ReviewLedger>,
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
        SlashCommand::Context => Ok(context_reply(engine)),
        SlashCommand::Distill => {
            distill_reply(
                engine,
                summarize,
                workspace,
                memory_provider,
                distiller,
                usage_ledger,
                distill::Gate::Bypass,
                "Nothing to distill.",
            )
            .await
        }
        SlashCommand::Duty { name } => {
            run_duty(
                &name,
                engine,
                summarize,
                workspace,
                task_review_provider,
                memory_provider,
                tools,
                distiller,
                max_iterations,
                memory_index_cap,
                usage_ledger,
                review_ledger.is_some(),
            )
            .await
        }
        SlashCommand::Duties => {
            all_duties(
                engine,
                summarize,
                workspace,
                task_review_provider,
                memory_provider,
                tools,
                distiller,
                max_iterations,
                memory_index_cap,
                usage_ledger,
                review_ledger.is_some(),
            )
            .await
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
        SlashCommand::Usage => match usage_ledger {
            None => Ok(Reply::text("Usage tracking is disabled.".into())),
            Some(ledger) => ledger
                .rows()
                .map(|rows| Reply::pre(usage::report(&rows)))
                .map_err(|e| format!("Usage query failed: {e}")),
        },
        SlashCommand::Findings => match review_ledger {
            None => Ok(Reply::text("Review tracking is disabled.".into())),
            Some(ledger) => ledger
                .report()
                .map(Reply::pre)
                .map_err(|e| format!("Findings query failed: {e}")),
        },
    }
}

/// Dispatch one named duty, gate respected (the scheduler's entry
/// point, `/duty <name>`).
#[allow(clippy::too_many_arguments)]
async fn run_duty(
    name: &str,
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    task_review_provider: &impl Provider,
    memory_provider: &impl Provider,
    tools: &Tools,
    distiller: &Distiller,
    max_iterations: usize,
    memory_index_cap: usize,
    usage_ledger: Option<&UsageLedger>,
    review_gates: bool,
) -> Result<Reply, String> {
    match name {
        "task-review" => task_review(
            engine,
            summarize,
            workspace,
            task_review_provider,
            tools,
            max_iterations,
            memory_index_cap,
            usage_ledger,
            review_gates,
        )
        .await
        .map(Reply::text),
        "distill" => {
            distill_reply(
                engine,
                summarize,
                workspace,
                memory_provider,
                distiller,
                usage_ledger,
                distill::Gate::Enforce,
                "Distillation gate closed.",
            )
            .await
        }
        other => Err(format!("Unknown duty: {other}")),
    }
}

/// Render the `/context` stats line.
fn context_reply(engine: &impl ContextEngine) -> Reply {
    let stats = engine.stats();
    let pct = (stats.token_estimate * 100)
        .checked_div(stats.budget)
        .unwrap_or(0);
    Reply::text(format!(
        "Context: {} / {} tokens ({pct}%)\n\
         Messages: {}\n\
         Session: {}",
        stats.token_estimate,
        stats.budget,
        stats.message_count,
        engine.active_session(),
    ))
}

/// One distillation pass rendered as a reply; `idle` is the message
/// when no pass ran (the wording differs per gate).
#[allow(clippy::too_many_arguments)]
async fn distill_reply(
    engine: &impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    memory_provider: &impl Provider,
    distiller: &Distiller,
    usage_ledger: Option<&UsageLedger>,
    gate: distill::Gate,
    idle: &str,
) -> Result<Reply, String> {
    let session = engine.active_session().to_string();
    let pass = distill_pass(
        engine,
        summarize,
        workspace,
        memory_provider,
        distiller,
        usage_ledger,
        &session,
        gate,
    )
    .await
    .map_err(|e| format!("Distillation failed: {e}"))?;
    match pass {
        Some(summary) => Ok(Reply::text(format!("Distilled: {summary}"))),
        None => Ok(Reply::text(idle.into())),
    }
}

/// The task-review duty: run the HEARTBEAT.md standing tasks as a
/// turn on the active session. Skips when there are none.
#[allow(clippy::too_many_arguments)]
async fn task_review(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    task_review_provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    memory_index_cap: usize,
    usage_ledger: Option<&UsageLedger>,
    review_gates: bool,
) -> Result<String, String> {
    let session = engine.active_session().to_string();
    match heartbeat::prepare(workspace) {
        Ok(heartbeat::Prepared::Ready(prompt)) => {
            let (output, usage) = agent::process_message_metered(
                engine,
                summarize,
                workspace,
                &prompt,
                task_review_provider,
                tools,
                max_iterations,
                memory_index_cap,
                review_gates,
                &crate::tools::ToolCtx::default(),
            )
            .await
            .map_err(|e| format!("Task review failed: {e}"))?;
            usage::record_turn(
                usage_ledger,
                &TurnRecord {
                    session: &session,
                    source: "task-review",
                    model: task_review_provider.model(),
                    usage,
                },
            );
            let response = output.into_text();
            if let Err(e) = heartbeat::finish(workspace, &response) {
                error!("Failed to write task-review history: {e}");
            }
            Ok(response)
        }
        Ok(heartbeat::Prepared::Skipped(reason)) => Ok(format!("Skipped: {reason}")),
        Err(e) => Err(format!("Task review failed: {e}")),
    }
}

/// Run every duty whose gate is open, ignoring schedules — the
/// operator's "run it now" (`/duties`). Duty failures degrade to
/// lines in the combined reply; the command itself succeeds.
#[allow(clippy::too_many_arguments)]
async fn all_duties(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    task_review_provider: &impl Provider,
    memory_provider: &impl Provider,
    tools: &Tools,
    distiller: &Distiller,
    max_iterations: usize,
    memory_index_cap: usize,
    usage_ledger: Option<&UsageLedger>,
    review_gates: bool,
) -> Result<Reply, String> {
    let review = task_review(
        engine,
        summarize,
        workspace,
        task_review_provider,
        tools,
        max_iterations,
        memory_index_cap,
        usage_ledger,
        review_gates,
    )
    .await
    .unwrap_or_else(|e| e);

    let session = engine.active_session().to_string();
    let distill_note = match distill_pass(
        &*engine,
        summarize,
        workspace,
        memory_provider,
        distiller,
        usage_ledger,
        &session,
        distill::Gate::Enforce,
    )
    .await
    {
        Ok(Some(summary)) => format!("\n\nDistilled: {summary}"),
        Ok(None) => String::new(),
        Err(e) => {
            error!("Distillation failed: {e}");
            String::new()
        }
    };
    Ok(Reply::text(format!("{review}{distill_note}")))
}

/// Run one distillation pass plus its bookkeeping: bill the turn to
/// `session` in the ledger and append the result to HISTORY.md.
/// Returns the pass summary, or `None` when no pass ran.
#[allow(clippy::too_many_arguments)]
async fn distill_pass(
    engine: &impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    memory_provider: &impl Provider,
    distiller: &Distiller,
    usage_ledger: Option<&UsageLedger>,
    session: &str,
    gate: distill::Gate,
) -> Result<Option<String>, String> {
    let mut state = distill::DistillState::load(&workspace.distillation_state_path());
    let out = distill::run(
        engine,
        distiller,
        memory_provider,
        summarize,
        workspace,
        &mut state,
        gate,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(out.map(|(summary, usage)| {
        usage::record_turn(
            usage_ledger,
            &TurnRecord {
                session,
                source: "distill",
                model: memory_provider.model(),
                usage,
            },
        );
        if let Err(e) = heartbeat::finish(workspace, &format!("Distilled: {summary}")) {
            error!("Failed to write distillation history: {e}");
        }
        summary
    }))
}

/// Dispatch `/project` with or without a name argument.
async fn project(engine: &mut impl ContextEngine, name: Option<String>) -> Result<Reply, String> {
    match name {
        None => list_projects(engine).await,
        Some(raw) => switch_project(engine, &raw).await,
    }
}

/// Render the session list, marking the active one. Names are compared
/// in sanitized space: engines list desanitized names but report the
/// active session in stored (sanitized) form.
fn render_sessions(sessions: &[SessionInfo], active: &str) -> String {
    let mut out = String::new();
    for s in sessions {
        let marker = if sanitize_name(&s.name) == active {
            "* "
        } else {
            "  "
        };
        let _ = writeln!(
            out,
            "{marker}{} ({} messages, ~{} tokens)",
            s.name, s.message_count, s.estimated_tokens,
        );
    }
    if out.is_empty() {
        out.push_str("No sessions.\n");
    }
    out
}

async fn list_projects(engine: &mut impl ContextEngine) -> Result<Reply, String> {
    let sessions = engine.list_sessions().await.map_err(|e| e.to_string())?;
    let out = render_sessions(&sessions, engine.active_session());
    Ok(Reply::pre(out))
}

/// Switch to an existing session. A name that matches no session is an
/// error, not a create: /project is navigation, and a typo silently
/// spawning a fresh session strands the user outside their context.
/// Sessions are created by channel routing, never from here.
async fn switch_project(engine: &mut impl ContextEngine, name: &str) -> Result<Reply, String> {
    let sessions = engine.list_sessions().await.map_err(|e| e.to_string())?;
    let target = sanitize_name(name);
    if !sessions.iter().any(|s| sanitize_name(&s.name) == target) {
        return Err(format!(
            "No session '{name}'. Sessions:\n{}",
            render_sessions(&sessions, engine.active_session()),
        ));
    }
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
    use crate::config::ContextConfig;
    use crate::engine::flat::FlatSession;
    use crate::types::Message;

    async fn engine_with_sessions(dir: &std::path::Path, names: &[&str]) -> FlatSession {
        let sessions_dir = dir.join("sessions");
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut engine =
            FlatSession::new(sessions_dir, state_dir, ContextConfig::default()).unwrap();
        for name in names {
            engine.switch_session(name).await.unwrap();
            engine
                .push_message(Message::User {
                    content: "hi".into(),
                })
                .await
                .unwrap();
            engine.save().await.unwrap();
        }
        engine
    }

    #[tokio::test]
    async fn switch_project_rejects_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = engine_with_sessions(dir.path(), &["general"]).await;

        let err = switch_project(&mut engine, "open-money").await.unwrap_err();
        assert!(err.contains("No session 'open-money'"), "{err}");
        assert!(err.contains("general"), "{err}");
        // The typo must not create a session.
        let sessions = engine.list_sessions().await.unwrap();
        assert!(!sessions.iter().any(|s| s.name.contains("open-money")));
        assert_eq!(engine.active_session(), "general");
    }

    #[tokio::test]
    async fn switch_project_accepts_existing_repo_style_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine =
            engine_with_sessions(dir.path(), &["CumuloGlobal/open-money", "general"]).await;

        let reply = switch_project(&mut engine, "CumuloGlobal/open-money")
            .await
            .unwrap();
        assert_eq!(engine.active_session(), "CumuloGlobal--open-money");
        assert!(reply.content.contains("Switched to"), "{}", reply.content);
    }

    #[tokio::test]
    async fn list_projects_marks_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine =
            engine_with_sessions(dir.path(), &["CumuloGlobal/open-money", "general"]).await;
        engine
            .switch_session("CumuloGlobal/open-money")
            .await
            .unwrap();

        let reply = list_projects(&mut engine).await.unwrap();
        assert!(reply.preformatted);
        let out = reply.content;
        assert!(out.contains("* CumuloGlobal/open-money"), "{out}");
        assert!(out.contains("  general"), "{out}");
    }

    #[test]
    fn parse_known_commands() {
        assert_eq!("/compact".parse(), Ok(SlashCommand::Compact));
        assert_eq!("/context".parse(), Ok(SlashCommand::Context));
        assert_eq!("/distill".parse(), Ok(SlashCommand::Distill));
        assert_eq!("/duties".parse(), Ok(SlashCommand::Duties));
        assert_eq!(
            "/duty task-review".parse(),
            Ok(SlashCommand::Duty {
                name: "task-review".into()
            }),
        );
        assert_eq!("/duty".parse::<SlashCommand>(), Err(UnknownCommand));
        assert_eq!("/heartbeat".parse::<SlashCommand>(), Err(UnknownCommand));
        assert_eq!("/new".parse(), Ok(SlashCommand::New));
        assert_eq!("/stats".parse(), Ok(SlashCommand::Stats));
        assert_eq!("/usage".parse(), Ok(SlashCommand::Usage));
        assert_eq!("/findings".parse(), Ok(SlashCommand::Findings));
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
