//! Slash command definitions shared across channels.
//!
//! Execution logic lives here so every channel behaves identically.
//! Input classification and routing lives in [`crate::dispatch`].

use std::fmt::Write as _;
use std::str::FromStr;

use tracing::error;

use crate::context::names::{display_name, sanitize_name};
use crate::context::{ContextEngine, SessionInfo, SummarizeFn};
use crate::dispatch::Reply;
use crate::memory::distill::{self, Distiller};
use crate::provider::Provider;
use crate::review::ReviewLedger;
use crate::usage::{self, TaskKey, TurnRecord, UsageLedger};
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
    /// Show recorded cost, broken down by task, build, and model.
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
/// Called by the agent actor. The memory provider is consumed only by
/// distillation, which runs on its own model.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cmd: SlashCommand,
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    memory_provider: &impl Provider,
    distiller: &Distiller,
    usage_ledger: Option<&UsageLedger>,
    review_ledger: Option<&ReviewLedger>,
    task: &TaskKey,
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
                task,
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
                memory_provider,
                distiller,
                usage_ledger,
                task,
            )
            .await
        }
        // Distillation is the only built-in duty until prompt duties
        // land; "all open gates" and "the distill gate" coincide.
        SlashCommand::Duties => {
            distill_reply(
                engine,
                summarize,
                workspace,
                memory_provider,
                distiller,
                usage_ledger,
                task,
                distill::Gate::Enforce,
                "Distillation gate closed.",
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
        SlashCommand::Stats => {
            let mut out = engine.report().await.map_err(|e| e.to_string())?;
            // Message-derived tables miss sub-agent turns (ephemeral
            // engine); the tee section covers them.
            out.push_str(&crate::context::stats::tee_section(&workspace.errors_dir()));
            Ok(Reply::pre(out))
        }
        SlashCommand::Usage => match usage_ledger {
            None => Ok(Reply::text("Usage tracking is disabled.".into())),
            Some(ledger) => match ledger.rows() {
                Ok(rows) => {
                    let live = ledger.live_rates(&rows).await;
                    Ok(Reply::pre(usage::report(&rows, ledger.rates(), &live)))
                }
                Err(e) => Err(format!("Usage query failed: {e}")),
            },
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
    memory_provider: &impl Provider,
    distiller: &Distiller,
    usage_ledger: Option<&UsageLedger>,
    task: &TaskKey,
) -> Result<Reply, String> {
    match name {
        "distill" => {
            distill_reply(
                engine,
                summarize,
                workspace,
                memory_provider,
                distiller,
                usage_ledger,
                task,
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
        display_name(engine.active_session()),
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
    task: &TaskKey,
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
        task,
        &session,
        gate,
    )
    .await
    .map_err(|e| format!("Distillation failed: {e}"))?;
    match pass {
        Some(summary) => Ok(Reply::text(format!("Distilled: {summary}"))),
        // A closed gate or empty backlog is mechanics, not an event:
        // the journal skips routine replies.
        None => Ok(Reply::routine(idle.into())),
    }
}

/// Run one distillation pass plus its bookkeeping: bill the turn to
/// `session` in the ledger and journal the result.
/// Returns the pass summary, or `None` when no pass ran.
#[allow(clippy::too_many_arguments)]
async fn distill_pass(
    engine: &impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    memory_provider: &impl Provider,
    distiller: &Distiller,
    usage_ledger: Option<&UsageLedger>,
    task: &TaskKey,
    session: &str,
    gate: distill::Gate,
) -> Result<Option<String>, String> {
    let mut state = distiller
        .load_state(engine)
        .await
        .map_err(|e| format!("Distillation state load failed: {e}"))?;
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
    Ok(out.map(|(summary, meter)| {
        usage::record_turn(
            usage_ledger,
            &TurnRecord {
                session,
                source: "distill",
                model: memory_provider.model(),
                task: Some(task),
                meter,
            },
        );
        if let Err(e) = crate::workspace::journal(
            &workspace.journal_path(),
            "distill",
            &format!("Distilled: {summary}"),
        ) {
            error!("Failed to journal distillation: {e}");
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
        display_name(engine.active_session()),
        engine.stats().message_count,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::context::flat::FlatSession;
    use crate::types::Message;

    async fn engine_with_sessions(dir: &std::path::Path, names: &[&str]) -> FlatSession {
        let mut engine = FlatSession::new(&dir.join("context"), ContextConfig::default()).unwrap();
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
        assert!(
            reply
                .content
                .contains("Switched to 'CumuloGlobal/open-money'"),
            "{}",
            reply.content
        );
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
