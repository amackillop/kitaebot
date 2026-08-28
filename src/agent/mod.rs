//! Core agent loop.
//!
//! Orchestrates the conversation between user, LLM, and tools.
//! Each turn sends context to the LLM and either returns a text response
//! or executes tool calls until the LLM completes.

mod actor;
pub(crate) mod envelope;
mod handle;
pub(crate) mod task;

pub use handle::AgentHandle;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::activity::{self, Activity};
use crate::agent::envelope::{ChannelSource, GitHubRole};
use crate::context::{ContextEngine, SummarizeFn};
use crate::error::{Error, ToolError};
use crate::provider::{CallUsage, Provider};
use crate::safety;
use crate::tools::{ToolCtx, Tools, truncate_output};
use crate::types::{Message, Response, ToolCall, ToolDefinition};
use crate::workspace::Workspace;

/// Byte cap on the error shown in the turn-summary log line. State
/// reports ride the error whole to the channel; the journal gets the
/// headline.
const TURN_SUMMARY_ERROR_MAX_BYTES: usize = 2_048;

/// Consecutive identical tool calls before execution is skipped.
const REPEAT_LIMIT: usize = 3;

/// Skipped rounds before the turn is abandoned. Skipping stops the tool
/// running but does nothing about a model that keeps asking: without a
/// limit the turn spends its whole budget re-sending one refused call.
const REPEAT_STRIKE_LIMIT: usize = 2;

const REPEAT_ERROR: &str = "ERROR: You have called this tool with identical \
    arguments multiple times and received the same result. \
    Do NOT retry the same call. Either use a different tool \
    or action, or respond to the user explaining what you \
    tried and why it did not work.";

/// Blocked errors from the same rule (identified by its guidance
/// string) before the turn is halted. Distinct rules strike
/// independently: the gate targets workarounds of one refusal, not a
/// long turn's unrelated first offenses.
const POLICY_STRIKE_LIMIT: usize = 2;

/// Blocked rounds in one turn, across all rules, before the turn is
/// halted anyway. Distinct rules learn independently below this cap;
/// a turn that keeps finding new walls is probing them, not learning.
const POLICY_ROUND_LIMIT: usize = 4;

/// Byte cap on message content in log lines.
const LOG_CONTENT_MAX: usize = 500;

const POLICY_STOP_DIRECTIVE: &str = "POLICY VIOLATION: A tool call was blocked. \
    Do NOT work around this. Report the situation to the user and await direction.";

/// Identical tool failures (same tool, canonical args, error class)
/// before the error text starts naming the repetition count.
const STRIKE_NOTICE: usize = 3;

/// Identical tool failures before the turn halts with a diagnosis
/// instead of grinding to `max_iterations`.
const STRIKE_HALT: usize = 5;

/// Outcome of a completed turn.
#[derive(Debug)]
pub enum TurnOutput {
    /// The model produced a final text response.
    Text(String),
    /// The turn was halted after repeated policy violations.
    PolicyHalt { reasons: Vec<String> },
    /// The turn was halted after a tool failed identically too many
    /// times — a deterministic failure the model kept retrying.
    ToolHalt {
        tool: String,
        args: String,
        error_class: String,
        count: usize,
    },
}

impl TurnOutput {
    /// Render the outcome as user-facing text.
    pub fn into_text(self) -> String {
        match self {
            Self::Text(s) => s,
            Self::PolicyHalt { reasons } => policy_halt_msg(&reasons),
            Self::ToolHalt {
                tool,
                args,
                error_class,
                count,
            } => tool_halt_msg(&tool, &args, &error_class, count),
        }
    }
}

fn policy_halt_msg(reasons: &[String]) -> String {
    use std::fmt::Write;
    let mut msg = String::from(
        "I attempted to use a blocked operation multiple times. \
         The turn was halted automatically.",
    );
    if !reasons.is_empty() {
        let _ = write!(msg, " Blocked: {}", reasons.join("; "));
    }
    msg.push_str(" Please advise how to proceed.");
    msg
}

fn tool_halt_msg(tool: &str, args: &str, error_class: &str, count: usize) -> String {
    format!(
        "A tool call failed identically {count} times this turn. The failure \
         is deterministic — retrying will not help. \
         Tool: {tool}, args: {args}, error class: {error_class}. \
         Please advise how to proceed."
    )
}

/// What the root system prompt needs beyond the cached static files:
/// the memory index cap (spec 21) and the repos whose own `AGENTS.md`
/// may be injected (spec 06). Both are per-turn assembly inputs, so
/// they travel together rather than as loose arguments.
pub(crate) struct PromptConfig {
    pub memory_index_cap: usize,
    pub trusted_repos: Vec<String>,
}

/// The developer workflow: clone through pull request. Split out of
/// `AGENTS.md` because it is builder choreography, and a turn spent
/// reviewing somebody else's PR should not be holding instructions on
/// how to push and open one.
const DEVELOPER_WORKFLOW: &str = include_str!("../prompts/developer-workflow.md");

/// Segments the bot builds with, and the ones it reviews with. Mutually
/// exclusive: the two modes are the same agent under different
/// instructions, not one agent holding both sets.
const BUILDER_SEGMENTS: &[&str] = &[DEVELOPER_WORKFLOW];
const REVIEWER_SEGMENTS: &[&str] = &[crate::channel::github::prs::REVIEW_PROTOCOL_SEGMENT];

/// Segments a dispatch carries (spec 06). Keyed on the dispatch rather
/// than the session: a session is where history accumulates, a role is
/// a property of the turn.
///
/// Only the reviewer role is knowable from a dispatch — the GitHub
/// channel knows which poll pass raised the item. Nothing declares a
/// turn to be build work, so builder is the default rather than a
/// detected mode.
pub(crate) fn role_segments(source: &ChannelSource) -> &'static [&'static str] {
    match source {
        ChannelSource::GitHub {
            role: GitHubRole::Reviewer,
            ..
        } => REVIEWER_SEGMENTS,
        _ => BUILDER_SEGMENTS,
    }
}

/// Run a single root turn, returning its outcome and billed
/// [`TurnUsage`] so the caller can record the cost to the usage ledger.
///
/// Shared by all root channels (telegram, socket, duties). Prepends
/// the memory index (spec 21) to the cached system prompt, read fresh so
/// runtime writes take effect on the next turn, then appends the
/// review-gates segment (spec 23) when the pipeline is enabled, the
/// segments the dispatched role carries (spec 06), and the
/// worked repo's conventions (spec 06) when the session names a
/// trusted clone. Sub-agents call [`run_turn_metered`] directly and are
/// excluded by design.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_message_metered(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    workspace: &Workspace,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    prompt: &PromptConfig,
    review_gates: bool,
    role_segments: &[&str],
    ctx: &ToolCtx,
) -> (Result<TurnOutput, Error>, TurnMeter) {
    let mut system_prompt =
        match crate::memory::index_segment(&workspace.memory_dir(), prompt.memory_index_cap) {
            Some(index) => format!("{}\n{index}", workspace.system_prompt()),
            None => workspace.system_prompt().to_string(),
        };
    if let Some(conventions) = crate::conventions::segment(
        workspace.path(),
        engine.active_session(),
        &prompt.trusted_repos,
    )
    .await
    {
        system_prompt.push_str(&conventions);
    }
    if review_gates {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(crate::review::GATES_SEGMENT);
    }
    for segment in role_segments {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(segment);
    }
    run_turn_metered(
        engine,
        summarize,
        &system_prompt,
        user_message,
        provider,
        tools,
        max_iterations,
        BudgetPolicy::Fail,
        ReplyPolicy::Accept,
        ctx,
    )
    .await
}

/// What the turn loop does when the iteration cap is hit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetPolicy {
    /// Squeeze a state report for a successor, then return
    /// [`Error::MaxIterationsReached`] carrying it: the turn fails
    /// (unattended alerts must fire) but its state survives.
    Fail,
    /// Take one final no-tools completion as a degraded answer.
    FinalAnswer,
}

/// What the turn loop does with a text response (no tool calls).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyPolicy {
    /// First text ends the turn; a human reads it as conversation.
    Accept,
    /// Text is held once: the loop pushes [`CONFIRM_REPLY_DIRECTIVE`]
    /// and continues, because the reply publishes verbatim to an
    /// external medium. The next text ends the turn.
    Confirm,
}

/// Counters for the end-of-turn summary line.
#[derive(Default)]
struct TurnStats {
    iterations: usize,
    tool_calls: usize,
    /// Last observed prompt size — the live context, not a sum.
    prompt_tokens: Option<usize>,
    /// Billed tokens and cost, summed across the turn's calls.
    usage: TurnUsage,
    /// The iteration cap was hit and the answer came from the
    /// final-answer squeeze.
    squeezed: bool,
    /// A text response was held for the [`ReplyPolicy::Confirm`] round.
    nudged: bool,
}

/// Billed usage for one turn, summed across its provider calls.
///
/// A turn is many calls (one per tool-round), so a single call's
/// numbers say nothing about the turn. This is the per-turn total the
/// ledger records.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnUsage {
    /// Provider calls made during the turn.
    pub calls: u32,
    /// Prompt tokens billed, summed across calls.
    pub prompt_tokens: u64,
    /// Prompt-cache hits, summed across the calls that reported them;
    /// `None` when no call did (same contract as `cost`).
    pub cached_tokens: Option<u64>,
    /// Tokens generated, summed across calls.
    pub completion_tokens: u64,
    /// Charged cost in USD, summed across calls; `None` when no call
    /// reported a cost (non-`OpenRouter`).
    pub cost: Option<f64>,
    /// Endpoint that served the turn's calls, last seen wins: sticky
    /// routing pins a session, so a mid-turn change is failover and
    /// the newest endpoint is the one to price against.
    pub provider: Option<String>,
}

impl TurnUsage {
    fn add_call(&mut self, call: CallUsage) {
        self.calls += 1;
        if let Some(prompt) = call.prompt_tokens {
            self.prompt_tokens += u64::from(prompt);
        }
        if let Some(cached) = call.cached_tokens {
            self.cached_tokens = Some(self.cached_tokens.unwrap_or(0) + u64::from(cached));
        }
        self.completion_tokens += u64::from(call.completion_tokens);
        if let Some(cost) = call.cost {
            self.cost = Some(self.cost.unwrap_or(0.0) + cost);
        }
        if call.provider.is_some() {
            self.provider = call.provider;
        }
    }
}

/// Everything the ledger records about one finished turn (spec 27):
/// billed usage plus wall time and the outcome label. Separate from
/// [`TurnUsage`] because that type's contract is call-summing (it has
/// a meaningful `Default` and `add_call`); a meter is measured once.
pub(crate) struct TurnMeter {
    pub usage: TurnUsage,
    /// Turn start, epoch seconds.
    pub started_at: u64,
    /// Wall time of the turn.
    pub duration: std::time::Duration,
    /// The label the turn summary logs; one derivation feeds both the
    /// log and the ledger.
    pub outcome: &'static str,
}

/// The turn-summary outcome label, shared by the log line and the
/// ledger row.
fn outcome_label(result: &Result<TurnOutput, Error>) -> &'static str {
    match result {
        Ok(TurnOutput::PolicyHalt { .. }) => "policy_halt",
        Ok(TurnOutput::Text(_)) => "text",
        Ok(TurnOutput::ToolHalt { .. }) => "tool_halt",
        Err(Error::Cancelled) => "cancelled",
        Err(Error::MaxIterationsReached { .. }) => "max_iterations",
        Err(Error::NoProgress) => "no_progress",
        Err(_) => "error",
    }
}

/// Test-only wrapper over [`run_turn_metered`] discarding the billed
/// usage, so the turn-loop tests stay off the tuple return.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    system_prompt: &str,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    ctx: &ToolCtx,
) -> Result<TurnOutput, Error> {
    run_turn_metered(
        engine,
        summarize,
        system_prompt,
        user_message,
        provider,
        tools,
        max_iterations,
        BudgetPolicy::Fail,
        ReplyPolicy::Accept,
        ctx,
    )
    .await
    .0
}

/// Run a single turn of the agent loop, returning its outcome and the
/// [`TurnMeter`] so callers can record cost, wall time, and the
/// outcome label to the usage ledger.
///
/// Pushes the user message onto the session, sends the history (with system
/// prompt prepended) to the provider, and appends assistant/tool messages.
/// The system prompt is assembled once at workspace init: the persona is
/// compiled in (`include_str!`), only the operator `USER.md` is read from
/// disk, so prompt changes need a restart (a rebuild for the persona).
///
/// Emits one INFO summary event per turn: outcome, iterations, tool
/// calls, last observed prompt tokens, and duration.
///
/// Exposed crate-internally so sub-agents (spec 19) run the exact
/// same loop against an ephemeral child context.
///
/// Billed usage is returned alongside the outcome, not inside it: the
/// calls happened whether or not the turn succeeded, and a failure is
/// where the cost most needs recording. Returning
/// `Result<(_, TurnUsage)>` dropped the usage on every error path, so a
/// turn that ground to the iteration cap — the most expensive way to
/// fail — billed nothing at all.
///
/// # Errors
/// Returns error if max iterations reached (under [`BudgetPolicy::Fail`])
/// or provider fails
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn_metered(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    system_prompt: &str,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    budget: BudgetPolicy,
    reply: ReplyPolicy,
    ctx: &ToolCtx,
) -> (Result<TurnOutput, Error>, TurnMeter) {
    let started_at = crate::time::now_epoch();
    let started = std::time::Instant::now();
    let mut stats = TurnStats::default();
    let result = turn_loop(
        engine,
        summarize,
        system_prompt,
        user_message,
        provider,
        tools,
        max_iterations,
        budget,
        reply,
        ctx,
        &mut stats,
    )
    .await;
    let elapsed = started.elapsed();
    let outcome = outcome_label(&result);
    match &result {
        Ok(_) => info!(
            outcome,
            iterations = stats.iterations,
            tool_calls = stats.tool_calls,
            prompt_tokens = stats.prompt_tokens,
            calls = stats.usage.calls,
            completion_tokens = stats.usage.completion_tokens,
            cost = stats.usage.cost,
            squeezed = stats.squeezed,
            nudged = stats.nudged,
            ?elapsed,
            "Turn summary"
        ),
        Err(e) => info!(
            outcome,
            // Bounded copy: the full text (state reports included)
            // reaches the channel; journald cuts long lines silently.
            error = %truncate_output(&e.to_string(), TURN_SUMMARY_ERROR_MAX_BYTES),
            iterations = stats.iterations,
            tool_calls = stats.tool_calls,
            prompt_tokens = stats.prompt_tokens,
            calls = stats.usage.calls,
            completion_tokens = stats.usage.completion_tokens,
            cost = stats.usage.cost,
            squeezed = stats.squeezed,
            nudged = stats.nudged,
            ?elapsed,
            "Turn summary"
        ),
    }
    let meter = TurnMeter {
        usage: stats.usage,
        started_at,
        duration: elapsed,
        outcome,
    };
    (result, meter)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn turn_loop(
    engine: &mut impl ContextEngine,
    summarize: &SummarizeFn,
    system_prompt: &str,
    user_message: &str,
    provider: &impl Provider,
    tools: &Tools,
    max_iterations: usize,
    budget: BudgetPolicy,
    reply: ReplyPolicy,
    ctx: &ToolCtx,
    stats: &mut TurnStats,
) -> Result<TurnOutput, Error> {
    let activity_tx = ctx.activity.as_ref();
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        activity::emit(activity_tx, Activity::Cancelled);
        return Err(Error::Cancelled);
    }

    debug!(content = %truncate_output(user_message, LOG_CONTENT_MAX), "Turn started");
    engine
        .push_message(Message::User {
            content: user_message.to_string(),
        })
        .await?;

    let tool_definitions: Arc<[ToolDefinition]> = tools.definitions().into();

    let mut repeats = RepeatDetector::new();
    let mut policy_strikes: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked_rounds: usize = 0;
    let mut repeat_strikes: usize = 0;
    let mut tool_strikes = ToolStrikeTracker::default();

    for iteration in 0..max_iterations {
        if cancel.is_cancelled() {
            activity::emit(activity_tx, Activity::Cancelled);
            return Err(Error::Cancelled);
        }

        stats.iterations = iteration + 1;
        debug!(iteration, "Agent loop iteration");

        // Emergency only: firing here cold-starts the prompt cache
        // for the rest of the turn. Routine compaction happens between
        // turns (actor, post-reply), where the damage is bounded at
        // one completion.
        if let Some(event) = engine.compact_if_urgent(summarize).await? {
            activity::emit(
                activity_tx,
                Activity::Compaction {
                    before: event.before,
                    after: event.after,
                },
            );
        }

        let assembled = engine.assemble(system_prompt).await?;

        let outcome = cancellable(
            provider.chat(
                engine.active_session(),
                &assembled.messages,
                &tool_definitions,
            ),
            cancel,
            activity_tx,
        )
        .await?
        .map_err(Error::Provider)?;

        engine.observe_request(assembled.messages, Arc::clone(&tool_definitions));

        let call = outcome.usage;
        if let Some(prompt_tokens) = call.prompt_tokens {
            engine.observe_tokens(prompt_tokens as usize);
            stats.prompt_tokens = Some(prompt_tokens as usize);
        }
        stats.usage.add_call(call);

        match outcome.response {
            Response::Text(content) => {
                engine
                    .push_message(Message::Assistant {
                        content: content.clone(),
                    })
                    .await?;
                // Never nudge into the cap: publishing possible
                // narration beats losing the turn to `BudgetPolicy::Fail`.
                if reply == ReplyPolicy::Confirm && !stats.nudged && iteration + 1 < max_iterations
                {
                    stats.nudged = true;
                    debug!(
                        content = %truncate_output(&content, LOG_CONTENT_MAX),
                        "Text held for publish confirmation"
                    );
                    engine
                        .push_message(Message::System {
                            content: CONFIRM_REPLY_DIRECTIVE.to_string(),
                        })
                        .await?;
                    continue;
                }
                debug!(content = %truncate_output(&content, LOG_CONTENT_MAX), "Turn finished");
                return Ok(TurnOutput::Text(content));
            }
            Response::ToolCalls { content, calls } => {
                engine
                    .push_message(Message::ToolCalls {
                        content,
                        calls: calls.clone(),
                    })
                    .await?;

                if repeats.record(&calls) {
                    repeat_strikes += 1;
                    // Name the call: the whole point of this line is
                    // telling the reader what the model is stuck on, and
                    // the skipped calls never reach the tool logs.
                    let repeated = calls
                        .iter()
                        .map(|c| format!("{}({})", c.function.name, c.function.arguments))
                        .collect::<Vec<_>>()
                        .join(", ");
                    warn!(
                        iteration,
                        repeat_strikes,
                        repeated = %truncate_output(&repeated, LOG_CONTENT_MAX),
                        "Repeated tool calls detected, skipping execution"
                    );
                    // Every tool_call needs a result before the next
                    // completion, so the refusal is pushed even when
                    // this is the round that ends the turn.
                    for call in &calls {
                        engine
                            .push_message(Message::Tool {
                                call_id: call.id.clone(),
                                content: REPEAT_ERROR.to_string(),
                            })
                            .await?;
                    }
                    if repeat_strikes >= REPEAT_STRIKE_LIMIT {
                        warn!(iteration, "Repeat strike limit reached, abandoning turn");
                        return Err(Error::NoProgress);
                    }
                    continue;
                }
                // Executing anything at all is progress.
                repeat_strikes = 0;

                stats.tool_calls += calls.len();
                for call in &calls {
                    activity::emit(
                        activity_tx,
                        Activity::ToolStart {
                            tool: call.function.name.to_string(),
                        },
                    );
                }

                // Execute all tool calls in parallel
                let futures: Vec<_> = calls
                    .iter()
                    .map(|call| tools.execute(call, ctx.clone()))
                    .collect();
                let results = cancellable(join_all(futures), cancel, activity_tx).await?;

                let blocked: Vec<(String, String)> = results
                    .iter()
                    .filter_map(|r| match r {
                        Err(ToolError::Blocked {
                            operation,
                            guidance,
                        }) => Some((format!("{operation} ({guidance})"), guidance.clone())),
                        _ => None,
                    })
                    .collect();

                let tool_halt =
                    record_tool_results(engine, &calls, results, activity_tx, &mut tool_strikes)
                        .await;

                if let Some(halt) = tool_halt {
                    warn!(
                        tool = %match &halt {
                            TurnOutput::ToolHalt { tool, .. } => tool.as_str(),
                            _ => "",
                        },
                        "Tool strike limit reached, halting turn"
                    );
                    return Ok(halt);
                }

                if !blocked.is_empty() {
                    blocked_rounds += 1;
                    // Each rule counts once per round: parallel calls were
                    // issued before this round's directive could land.
                    let round: BTreeSet<&str> = blocked.iter().map(|(_, g)| g.as_str()).collect();
                    let mut halt = blocked_rounds >= POLICY_ROUND_LIMIT;
                    for guidance in round {
                        let strikes = policy_strikes.entry(guidance.to_string()).or_insert(0);
                        *strikes += 1;
                        halt |= *strikes >= POLICY_STRIKE_LIMIT;
                    }
                    if halt {
                        warn!(blocked_rounds, "Policy strike limit reached, halting turn");
                        return Ok(TurnOutput::PolicyHalt {
                            reasons: blocked.into_iter().map(|(reason, _)| reason).collect(),
                        });
                    }
                    engine
                        .push_message(Message::System {
                            content: POLICY_STOP_DIRECTIVE.to_string(),
                        })
                        .await?;
                }
            }
        }
    }

    activity::emit(activity_tx, Activity::MaxIterations);
    match budget {
        BudgetPolicy::Fail => Err(state_report(engine, system_prompt, provider, ctx, stats).await),
        BudgetPolicy::FinalAnswer => {
            final_answer(engine, system_prompt, provider, ctx, stats).await
        }
    }
}

/// The held-reply directive for the confirmation round under
/// [`ReplyPolicy::Confirm`].
const CONFIRM_REPLY_DIRECTIVE: &str = "Your previous message was not \
    published. You are working unattended; your next text reply will be \
    posted verbatim as a public comment. If the work is unfinished, \
    continue with tool calls. Otherwise reply with exactly the comment \
    to publish: what was done, what remains, and any branch or PR \
    created.";

/// The budget-exhausted directive for the final-answer squeeze.
const FINAL_ANSWER_DIRECTIVE: &str = "Your tool-call budget for this task is \
    exhausted; no further tool calls will be executed. Reply now with your \
    final answer from the evidence gathered so far, and state what remains \
    unverified.";

/// The successor-report directive for capped turns under
/// [`BudgetPolicy::Fail`].
const STATE_REPORT_DIRECTIVE: &str = "Your iteration budget is exhausted and \
    this turn is about to be reported as failed. No further tool calls will \
    be executed. Reply now with a state report for a successor: what you \
    were doing, what is complete (branch names and commits pushed or left \
    in the working tree), what remains, and the specific obstacle if you \
    were stuck. Concrete paths and refs over prose.";

/// Bytes of state report carried in the error; Telegram's message cap
/// is the binding consumer (spec 17).
const STATE_REPORT_MAX: usize = 3000;

/// One last no-tools completion after the iteration cap: a degraded
/// answer beats a lost turn.
async fn final_answer(
    engine: &mut impl ContextEngine,
    system_prompt: &str,
    provider: &impl Provider,
    ctx: &ToolCtx,
    stats: &mut TurnStats,
) -> Result<TurnOutput, Error> {
    let content = squeeze(
        engine,
        system_prompt,
        provider,
        ctx,
        stats,
        FINAL_ANSWER_DIRECTIVE,
    )
    .await?;
    Ok(TurnOutput::Text(content))
}

/// The state-at-exit squeeze for [`BudgetPolicy::Fail`]: always yields
/// `MaxIterationsReached` (a failed squeeze must not mask the cap, and
/// a successful one must not look like success — unattended alerts
/// fire on the error), except for cancellation, which stays its own
/// outcome.
async fn state_report(
    engine: &mut impl ContextEngine,
    system_prompt: &str,
    provider: &impl Provider,
    ctx: &ToolCtx,
    stats: &mut TurnStats,
) -> Error {
    match squeeze(
        engine,
        system_prompt,
        provider,
        ctx,
        stats,
        STATE_REPORT_DIRECTIVE,
    )
    .await
    {
        Ok(report) => Error::MaxIterationsReached {
            report: truncate_output(&report, STATE_REPORT_MAX).into_owned(),
        },
        Err(Error::Cancelled) => Error::Cancelled,
        Err(e) => Error::MaxIterationsReached {
            report: format!("(state report unavailable: {e})"),
        },
    }
}

/// One no-tools completion under `directive`, billed to the turn and
/// recorded in the session so a successor turn can see it.
async fn squeeze(
    engine: &mut impl ContextEngine,
    system_prompt: &str,
    provider: &impl Provider,
    ctx: &ToolCtx,
    stats: &mut TurnStats,
    directive: &str,
) -> Result<String, Error> {
    stats.squeezed = true;
    engine
        .push_message(Message::System {
            content: directive.to_string(),
        })
        .await?;
    let assembled = engine.assemble(system_prompt).await?;
    let outcome = cancellable(
        provider.chat(engine.active_session(), &assembled.messages, &[]),
        &ctx.cancel,
        ctx.activity.as_ref(),
    )
    .await?
    .map_err(Error::Provider)?;

    engine.observe_request(assembled.messages, Arc::from([]));

    let call = outcome.usage;
    if let Some(prompt_tokens) = call.prompt_tokens {
        engine.observe_tokens(prompt_tokens as usize);
        stats.prompt_tokens = Some(prompt_tokens as usize);
    }
    stats.usage.add_call(call);

    let content = match outcome.response {
        Response::Text(content) => content,
        Response::ToolCalls { content, .. } => {
            warn!("provider emitted tool calls with none offered; taking content");
            content
        }
    };
    debug!(content = %truncate_output(&content, LOG_CONTENT_MAX), "Turn finished (squeezed)");
    engine
        .push_message(Message::Assistant {
            content: content.clone(),
        })
        .await?;
    Ok(content)
}

// ── Private helpers ─────────────────────────────────────────────────

/// Tracks consecutive identical tool call sets to detect stuck loops.
struct RepeatDetector {
    prev: Option<Vec<(String, serde_json::Value)>>,
    count: usize,
}

impl RepeatDetector {
    fn new() -> Self {
        Self {
            prev: None,
            count: 0,
        }
    }

    /// Record a new set of tool calls. Returns `true` if the limit is reached.
    fn record(&mut self, calls: &[ToolCall]) -> bool {
        let fingerprint: Vec<(String, serde_json::Value)> = calls
            .iter()
            .map(|c| {
                let args = serde_json::from_str(&c.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(c.function.arguments.clone()));
                (c.function.name.to_string(), args)
            })
            .collect();

        if self.prev.as_ref() == Some(&fingerprint) {
            self.count += 1;
        } else {
            self.count = 1;
            self.prev = Some(fingerprint);
        }

        self.count >= REPEAT_LIMIT
    }
}

/// Race a future against a cancellation token.
///
/// Returns the future's output on completion, or `Err(Cancelled)` if the
/// token fires first. Emits `Activity::Cancelled` before returning.
async fn cancellable<T>(
    future: impl Future<Output = T>,
    cancel: &CancellationToken,
    activity_tx: Option<&mpsc::Sender<Activity>>,
) -> Result<T, Error> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            activity::emit(activity_tx, Activity::Cancelled);
            Err(Error::Cancelled)
        }
        output = future => Ok(output),
    }
}

/// Canonicalize tool arguments so semantically identical JSON
/// matches regardless of key order.
fn canonical_args(args: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => canonicalize_value(v).to_string(),
        Err(_) => args.to_string(),
    }
}

/// Recursively sort map keys so semantically identical JSON matches.
fn canonicalize_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<_> = map.into_iter().collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in keys {
                sorted.insert(k, canonicalize_value(v));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_value).collect())
        }
        other => other,
    }
}

/// Classify a tool error into a coarse key for strike grouping. The
/// key distinguishes "same tool, same args, same failure mode" from
/// "same tool, same args, different failure" — only the former is
/// deterministic and worth escalating.
fn error_class(err: &ToolError) -> String {
    match err {
        ToolError::Blocked { .. } => "blocked".into(),
        ToolError::Cancelled => "cancelled".into(),
        ToolError::CommandFailed { exit_code, .. } => format!("command_failed:{exit_code}"),
        ToolError::EditLoop { .. } => "edit_loop".into(),
        ToolError::FtsQuery { .. } => "fts_query".into(),
        ToolError::Github(e) => match e {
            crate::error::GithubError::Api { status, .. } => format!("github:api:{status}"),
            crate::error::GithubError::Deserialize(_) => "github:deserialize".into(),
            crate::error::GithubError::Network(_) => "github:network".into(),
            crate::error::GithubError::RateLimited { .. } => "github:rate_limited".into(),
        },
        ToolError::Linear(e) => match e {
            crate::error::LinearError::Api(_) => "linear:api".into(),
            crate::error::LinearError::Deserialize(_) => "linear:deserialize".into(),
            crate::error::LinearError::Network(_) => "linear:network".into(),
        },
        ToolError::Http { .. } => "http".into(),
        ToolError::HttpStatus { status, .. } => format!("http_status:{status}"),
        ToolError::InvalidArguments(_) => "invalid_arguments".into(),
        ToolError::Join(_) => "join".into(),
        ToolError::Io { .. } => "io".into(),
        ToolError::Mcp { .. } => "mcp".into(),
        ToolError::NotFound(_) => "not_found".into(),
        ToolError::Precondition(_) => "precondition".into(),
        ToolError::Sqlite { context, .. } => format!("sqlite:{context}"),
        ToolError::Spawn { .. } => "spawn".into(),
        ToolError::SubAgent { .. } => "sub_agent".into(),
        ToolError::Telegram(_) => "telegram".into(),
        ToolError::Timeout { .. } => "timeout".into(),
        ToolError::WebSearch(_) => "web_search".into(),
    }
}

/// Per-turn tracker for identical tool failures across non-consecutive
/// rounds. The repeat detector only catches *consecutive* identical
/// calls; a model that interleaves other calls escapes it. This tracker
/// survives interleaving for the whole turn, keyed on
/// (tool name, canonical args, error class).
#[derive(Default)]
struct ToolStrikeTracker {
    strikes: BTreeMap<(String, String, String), usize>,
}

impl ToolStrikeTracker {
    /// Record a failed tool call and return the strike count for this
    /// signature. `ToolError::Blocked` is excluded — already gated by
    /// the policy strike system.
    fn record(&mut self, tool: &str, args: &str, err: &ToolError) -> Option<usize> {
        if matches!(err, ToolError::Blocked { .. }) {
            return None;
        }
        let key = (tool.to_string(), canonical_args(args), error_class(err));
        let count = self.strikes.entry(key).or_insert(0);
        *count += 1;
        Some(*count)
    }

    /// Augment a tool error result with a repetition notice when the
    /// strike count reaches [`STRIKE_NOTICE`]. The augmentation appends
    /// after the `"Error: {Display}"` text, preserving the prefix that
    /// `classify_failure` matches on.
    fn augment_notice(content: &str, count: usize) -> String {
        if count >= STRIKE_NOTICE {
            format!(
                "{content}\n\nThis exact call has failed {count} times \
                 this turn. The failure is deterministic — stop retrying \
                 and adapt your approach or report the problem."
            )
        } else {
            content.to_string()
        }
    }

    /// Progress-preserving guidance for timeout errors: retries may
    /// resume further along (build artifacts persist in the target
    /// dir / nix store). At [`STRIKE_NOTICE`] the guidance adds a
    /// caveat: if the store has not advanced, the failure is
    /// deterministic.
    fn timeout_notice(command: &str, secs: u64, count: usize) -> String {
        let base = format!("Error: `{command}` timed out after {secs}s");
        if count >= STRIKE_NOTICE {
            format!(
                "{base}\n\nThis command has timed out {count} times this \
                 turn. Partial progress persists — a retry resumes where \
                 this stopped. But if the underlying store/target mtimes \
                 have not advanced, the failure is deterministic: stop \
                 retrying and report the problem."
            )
        } else {
            format!(
                "{base}\n\nPartial progress persists — a retry resumes \
                 where this stopped."
            )
        }
    }
}

/// Process tool execution results: check safety, emit events, record
/// to engine. Returns `Some(TurnOutput::ToolHalt)` when a tool failure
/// reaches the strike limit — after all results are recorded, so no
/// dangling tool calls are left in the context.
async fn record_tool_results<E: ContextEngine>(
    engine: &mut E,
    calls: &[ToolCall],
    results: Vec<Result<String, ToolError>>,
    activity_tx: Option<&mpsc::Sender<Activity>>,
    tool_strikes: &mut ToolStrikeTracker,
) -> Option<TurnOutput> {
    let mut halt: Option<TurnOutput> = None;
    for (call, result) in calls.iter().zip(results) {
        let (content, err) = match result {
            Ok(output) => {
                let checked = safety::check_tool_output(call.function.name.as_str(), &output);
                // WARN feeds the error tee; recurring redactions are
                // self-analysis symptoms (a real leak or a false positive).
                for pattern in &checked.redactions {
                    warn!(
                        tool = %call.function.name,
                        pattern,
                        "Tool output redacted: secret-shaped span withheld"
                    );
                }
                (checked.wrapped, None)
            }
            Err(e) => {
                error!(tool = %call.function.name, "Tool execution failed: {}", e.log_summary());
                let base = format!("Error: {e}");
                let count =
                    tool_strikes.record(call.function.name.as_str(), &call.function.arguments, &e);
                // Timeout errors get progress-preserving guidance;
                // all others get the standard strike augmentation.
                let content = match (&e, count) {
                    (ToolError::Timeout { command, secs, .. }, Some(n)) => {
                        ToolStrikeTracker::timeout_notice(command, *secs, n)
                    }
                    (_, Some(n)) => ToolStrikeTracker::augment_notice(&base, n),
                    (_, None) => base,
                };
                let err_str = e.to_string();
                if let Some(n) = count
                    && n >= STRIKE_HALT
                {
                    halt = Some(TurnOutput::ToolHalt {
                        tool: call.function.name.to_string(),
                        args: call.function.arguments.clone(),
                        error_class: error_class(&e),
                        count: n,
                    });
                }
                (content, Some(err_str))
            }
        };

        activity::emit(
            activity_tx,
            Activity::ToolEnd {
                tool: call.function.name.to_string(),
                error: err,
            },
        );

        // Ignore push_message errors in tool result recording -- the turn
        // will fail on the next assemble() call if the engine is broken.
        let _ = engine
            .push_message(Message::Tool {
                call_id: call.id.clone(),
                content,
            })
            .await;
    }
    halt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::context::flat::FlatSession;
    use crate::context::make_summarize_fn;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::{MockBlockedTool, MockFailingTool, MockTool, Tool};
    use crate::types::{ToolCall, ToolFunction};
    use std::sync::Arc;

    fn tx_ctx(tx: &mpsc::Sender<Activity>) -> ToolCtx {
        ToolCtx {
            activity: Some(tx.clone()),
            ..ToolCtx::default()
        }
    }

    #[test]
    fn turn_usage_sums_calls_tokens_and_cost() {
        let mut usage = TurnUsage::default();
        usage.add_call(CallUsage {
            prompt_tokens: Some(100),
            cached_tokens: Some(80),
            completion_tokens: 20,
            cost: Some(0.001),
            provider: None,
        });
        // A call with no usage still counts, contributes nothing else.
        usage.add_call(CallUsage::default());
        usage.add_call(CallUsage {
            prompt_tokens: Some(50),
            cached_tokens: Some(0),
            completion_tokens: 10,
            cost: Some(0.002),
            provider: None,
        });
        assert_eq!(usage.calls, 3);
        assert_eq!(usage.prompt_tokens, 150);
        assert_eq!(usage.cached_tokens, Some(80));
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.cost, Some(0.003));
    }

    /// Last seen wins, and a provider-less call (failover retry, or a
    /// non-OpenRouter response) never erases a known endpoint.
    #[test]
    fn turn_usage_provider_keeps_last_seen() {
        let mut usage = TurnUsage::default();
        usage.add_call(CallUsage {
            provider: Some("Sail Research".into()),
            ..CallUsage::default()
        });
        usage.add_call(CallUsage::default());
        assert_eq!(usage.provider.as_deref(), Some("Sail Research"));
        usage.add_call(CallUsage {
            provider: Some("Ambient".into()),
            ..CallUsage::default()
        });
        assert_eq!(usage.provider.as_deref(), Some("Ambient"));
    }

    #[test]
    fn turn_usage_cost_stays_none_without_any() {
        let mut usage = TurnUsage::default();
        usage.add_call(CallUsage {
            prompt_tokens: Some(10),
            cached_tokens: None,
            completion_tokens: 5,
            cost: None,
            provider: None,
        });
        assert_eq!(usage.calls, 1);
        assert_eq!(usage.cost, None);
        // No call reported cache details: absent, not zero.
        assert_eq!(usage.cached_tokens, None);
    }

    fn text(s: &str) -> Response {
        Response::Text(s.to_string())
    }

    fn mock_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock".parse().unwrap(),
                arguments: "{}".to_string(),
            },
        )
    }

    /// A distinct call per iteration: the repeat detector fingerprints
    /// on (name, arguments), so varying the arguments is what separates
    /// "burned the budget working" from "burned it repeating itself".
    fn mock_distinct_call(n: usize) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                format!("call-{n}"),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: format!("{{\"n\":{n}}}"),
                },
            )],
        }
    }

    fn mock_tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: ids.iter().map(|&id| mock_call(id)).collect(),
        }
    }

    fn mock_tools(output: &str) -> Tools {
        Tools::new(vec![Arc::new(MockTool::new(output))], &[]).unwrap()
    }

    const SYSTEM: &str = "You are a test assistant.";
    const MAX_ITER: usize = 20;

    fn test_engine() -> FlatSession {
        let dir = tempfile::tempdir().unwrap();
        #[allow(deprecated)]
        let base = dir.into_path();
        FlatSession::new(&base.join("context"), ContextConfig::default()).unwrap()
    }

    fn test_summarize(provider: &Arc<MockProvider>) -> SummarizeFn {
        make_summarize_fn(provider.clone())
    }

    #[tokio::test]
    async fn test_text_response() {
        let provider = Arc::new(MockProvider::new(vec![Ok(text("Hello from LLM"))]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Hello",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Hello from LLM");
        // User + Assistant messages stored
        assert_eq!(engine.stats().message_count, 2);
    }

    /// Mid-work narration without tool calls used to end the turn and
    /// get posted verbatim as a public comment (issue #45). Under
    /// `Confirm` the first text is held and the second one publishes.
    #[tokio::test]
    async fn confirm_holds_first_text_and_publishes_the_second() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(text("I see the issue: let me think...")),
            Ok(text("Status: fix pushed on branch xyz.")),
        ]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let (result, _) = run_turn_metered(
            &mut engine,
            &summarize,
            SYSTEM,
            "Fix the bug",
            &*provider,
            &tools,
            MAX_ITER,
            BudgetPolicy::Fail,
            ReplyPolicy::Confirm,
            &ToolCtx::default(),
        )
        .await;

        assert_eq!(
            result.unwrap().into_text(),
            "Status: fix pushed on branch xyz."
        );
        assert_eq!(provider.call_count(), 2);
        // User, held Assistant, System directive, final Assistant.
        assert_eq!(engine.stats().message_count, 4);
    }

    /// The nudge is a resume point, not a forced answer: a model that
    /// leaked narration mid-work picks its tools back up.
    #[tokio::test]
    async fn confirm_lets_the_model_resume_tool_work() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(text("hmm, the borrow checker...")),
            Ok(mock_tool_calls(&["c1"])),
            Ok(text("Done: edit applied.")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let (result, _) = run_turn_metered(
            &mut engine,
            &summarize,
            SYSTEM,
            "Fix the bug",
            &*provider,
            &tools,
            MAX_ITER,
            BudgetPolicy::Fail,
            ReplyPolicy::Confirm,
            &ToolCtx::default(),
        )
        .await;

        assert_eq!(result.unwrap().into_text(), "Done: edit applied.");
        assert_eq!(provider.call_count(), 3);
    }

    /// A nudge on the last iteration would push the turn past the cap
    /// and lose it under `BudgetPolicy::Fail`; the text is accepted.
    #[tokio::test]
    async fn confirm_never_nudges_into_the_iteration_cap() {
        let provider = Arc::new(MockProvider::new(vec![Ok(text("only response"))]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let (result, _) = run_turn_metered(
            &mut engine,
            &summarize,
            SYSTEM,
            "Fix the bug",
            &*provider,
            &tools,
            1,
            BudgetPolicy::Fail,
            ReplyPolicy::Confirm,
            &ToolCtx::default(),
        )
        .await;

        assert_eq!(result.unwrap().into_text(), "only response");
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn test_tool_call_execution() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1"])),
            Ok(text("Tool result processed")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Use a tool",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Tool result processed");
    }

    /// The refusal alone does not stop a model that keeps asking. Without
    /// a strike limit the turn spends its entire budget re-sending one
    /// call, which is how a live turn burned 76 of 100 iterations.
    #[tokio::test]
    async fn a_model_that_will_not_stop_repeating_ends_the_turn() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["c1"]));
            MAX_ITER
        ]));
        let tools = mock_tools("same output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let (result, usage) = run_turn_metered(
            &mut engine,
            &summarize,
            SYSTEM,
            "Stuck",
            &*provider,
            &tools,
            MAX_ITER,
            BudgetPolicy::Fail,
            ReplyPolicy::Accept,
            &ToolCtx::default(),
        )
        .await;

        assert!(matches!(result.unwrap_err(), Error::NoProgress));
        // Two executed, two refused, then out: nowhere near the cap.
        assert_eq!(provider.call_count(), 4);
        assert_eq!(usage.usage.calls, 4, "the wasted calls are still billed");
        assert_eq!(usage.outcome, "no_progress");
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let responses = (0..MAX_ITER)
            .map(|n| Ok(mock_distinct_call(n)))
            // The state-report squeeze after the cap.
            .chain([Ok(text("was rewiring the frobnicator on branch kb-62"))])
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Infinite loop",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        let err = result.unwrap_err();
        assert!(matches!(err, Error::MaxIterationsReached { .. }));
        let text = err.to_string();
        assert!(text.contains("Maximum iterations reached"));
        assert!(text.contains("State at exit:"));
        assert!(text.contains("frobnicator on branch kb-62"));
    }

    /// A provider failure on the report squeeze must not mask the cap:
    /// the turn still reports `max_iterations`, just without state.
    #[tokio::test]
    async fn cap_report_falls_back_when_squeeze_fails() {
        let responses = (0..MAX_ITER)
            .map(|n| Ok(mock_distinct_call(n)))
            .chain([Err(ProviderError::Network("connection reset".into()))])
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Infinite loop",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        let err = result.unwrap_err();
        assert!(matches!(err, Error::MaxIterationsReached { .. }));
        assert!(err.to_string().contains("state report unavailable"));
    }

    /// The report rides in comments and Telegram pushes; it must be
    /// bounded no matter what the model produces.
    #[tokio::test]
    async fn cap_report_is_truncated() {
        let responses = (0..MAX_ITER)
            .map(|n| Ok(mock_distinct_call(n)))
            .chain([Ok(text(&"x".repeat(10_000)))])
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Infinite loop",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        let text = result.unwrap_err().to_string();
        assert!(text.contains("[truncated"));
        assert!(text.len() < 4000, "report must stay Telegram-sized");
    }

    /// Grinding to the iteration cap is the most expensive way for a turn
    /// to fail, so it is the one that most needs billing. Usage used to
    /// ride inside the `Ok`, which meant this path recorded nothing.
    #[tokio::test]
    async fn a_capped_turn_still_reports_its_usage() {
        let responses = (0..MAX_ITER)
            .map(|n| Ok(mock_distinct_call(n)))
            .chain([Ok(text("state report"))])
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let (result, usage) = run_turn_metered(
            &mut engine,
            &summarize,
            SYSTEM,
            "Infinite loop",
            &*provider,
            &tools,
            MAX_ITER,
            BudgetPolicy::Fail,
            ReplyPolicy::Accept,
            &ToolCtx::default(),
        )
        .await;

        assert!(matches!(
            result.unwrap_err(),
            Error::MaxIterationsReached { .. }
        ));
        // The report squeeze is one extra call, billed like the rest.
        assert_eq!(
            usage.usage.calls,
            u32::try_from(MAX_ITER + 1).unwrap(),
            "every call the turn made should be billed"
        );
        assert_eq!(usage.outcome, "max_iterations");
        assert!(usage.started_at > 0);
    }

    #[tokio::test]
    async fn test_repeated_tool_calls_skipped() {
        // Two executed, the third skipped, then something different —
        // which clears the strike and lets the turn finish.
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["c1"])),
            Ok(mock_tool_calls(&["c2"])),
            Ok(mock_tool_calls(&["c3"])),
            Ok(mock_distinct_call(99)),
            Ok(text("Gave up")),
        ]));
        let tools = mock_tools("same output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Loop test",
            &*provider,
            &tools,
            MAX_ITER,
            &tx_ctx(&tx),
        )
        .await;

        assert_eq!(result.unwrap().into_text(), "Gave up");
        assert_eq!(provider.call_count(), 5);

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolStart { .. }))
            .count();
        let tool_ends = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolEnd { .. }))
            .count();
        // c1, c2, then the distinct call; c3 was skipped.
        assert_eq!(tool_starts, 3);
        assert_eq!(tool_ends, 3);

        // Assembled context should contain repetition error messages for skipped calls.
        let ctx = engine.assemble(SYSTEM).await.unwrap();
        let repetition_msgs: Vec<_> = ctx
            .messages
            .iter()
            .filter(|m| {
                matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called"))
            })
            .collect();
        assert_eq!(repetition_msgs.len(), 1); // only the third call was refused
    }

    #[tokio::test]
    async fn test_different_tool_calls_not_flagged() {
        let call_a = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c1".to_string(),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: r#"{"x":1}"#.to_string(),
                },
            )],
        };
        let call_b = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c2".to_string(),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: r#"{"x":2}"#.to_string(),
                },
            )],
        };
        let provider = Arc::new(MockProvider::new(vec![
            Ok(call_a),
            Ok(call_b),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "No repeat",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Done");

        // No repetition error messages in assembled context.
        let ctx = engine.assemble(SYSTEM).await.unwrap();
        let repetition_msgs: Vec<_> = ctx
            .messages
            .iter()
            .filter(|m| {
                matches!(m, Message::Tool { content, .. } if content.starts_with("ERROR: You have called"))
            })
            .collect();
        assert!(repetition_msgs.is_empty());
    }

    #[tokio::test]
    async fn test_repeat_counter_resets_on_different_call() {
        let call_a = || Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: r#"{"v":"a"}"#.to_string(),
                },
            )],
        };
        let call_b = || Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "id".to_string(),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: r#"{"v":"b"}"#.to_string(),
                },
            )],
        };
        let provider = Arc::new(MockProvider::new(vec![
            Ok(call_a()),
            Ok(call_a()),
            Ok(call_b()),
            Ok(call_b()),
            Ok(call_b()),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Reset test",
            &*provider,
            &tools,
            MAX_ITER,
            &tx_ctx(&tx),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Done");

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, Activity::ToolStart { .. }))
            .count();
        assert_eq!(tool_starts, 4);
    }

    #[tokio::test]
    async fn test_provider_error() {
        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::Network(
            "Mock error".to_string(),
        ))]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Error case",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Provider(_)));
    }

    #[tokio::test]
    async fn test_parallel_tool_calls() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Multiple tools executed")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Parallel tools",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Multiple tools executed");
    }

    #[tokio::test]
    async fn test_safety_redacts_leaked_secret() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-leak"])),
            Ok(text("Handled")),
        ]));
        let tools = mock_tools("Here is your key: sk-proj-abc123def456ghi789jkl012");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Leak test",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await
        .unwrap();

        // Assemble to inspect messages (system prompt + session messages)
        let ctx = engine.assemble("").await.unwrap();
        let tool_msg = ctx
            .messages
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .expect("should have a tool message");

        if let Message::Tool { content, .. } = tool_msg {
            // The span is withheld; the surrounding output survives.
            assert!(!content.contains("sk-proj-abc123def456ghi789jkl012"));
            assert!(content.contains("Here is your key: [REDACTED: OpenAI API key]"));
            assert!(content.contains("<tool_output name=\"mock\">"));
        }
    }

    #[tokio::test]
    async fn test_clean_tool_output_wrapped() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1"])),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Wrap test",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await
        .unwrap();

        let ctx = engine.assemble("").await.unwrap();
        let tool_msg = ctx
            .messages
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .expect("should have a tool message");

        if let Message::Tool { content, .. } = tool_msg {
            assert!(content.contains("<tool_output name=\"mock\">"));
            assert!(content.contains("</tool_output>"));
        }
    }

    /// Pins the string contract between `record_tool_results` (producer)
    /// and `stats::classify_failure` (consumer). There is no shared type:
    /// errors are stored as `Error: {ToolError}` via Display. If either
    /// side drifts, the /stats failure tables silently go blind --
    /// exactly what happened when `Blocked` was misclassified as
    /// success. Safety redactions are successes and classify as None.
    #[tokio::test]
    async fn stored_tool_results_round_trip_stats_classification() {
        use crate::context::stats::{FailureKind, classify_failure};

        let cases: Vec<(Result<String, ToolError>, Option<FailureKind>)> = vec![
            (Ok("plain output".into()), None),
            (Ok("key is sk-proj-abc123def456ghi789jkl012".into()), None),
            (
                Err(ToolError::Blocked {
                    operation: "git push origin main".into(),
                    guidance: "use the git_push tool".into(),
                }),
                Some(FailureKind::Blocked),
            ),
            (
                Err(ToolError::EditLoop {
                    path: "src/main.rs".into(),
                    attempts: 3,
                    outcome: crate::error::EditFutility::NoChange,
                }),
                Some(FailureKind::EditLoop),
            ),
            (
                Err(ToolError::Precondition("exit 1".into())),
                Some(FailureKind::Precondition),
            ),
            (
                Err(ToolError::InvalidArguments("missing field".into())),
                Some(FailureKind::InvalidArguments),
            ),
            (
                Err(ToolError::NotFound("bogus".into())),
                Some(FailureKind::NotFound),
            ),
            (
                Err(ToolError::Spawn {
                    argv: "/proc/self/exe confine git /ws -- git ls-remote".into(),
                    cwd: "/ws".into(),
                    source: std::io::Error::from_raw_os_error(13),
                }),
                Some(FailureKind::Spawn),
            ),
            (
                Err(ToolError::Timeout {
                    command: "sleep 99".into(),
                    secs: 5,
                    evidence: crate::error::TimeoutEvidence::default(),
                }),
                Some(FailureKind::Timeout),
            ),
        ];
        let (results, expected): (Vec<_>, Vec<_>) = cases.into_iter().unzip();
        let calls: Vec<ToolCall> = (0..results.len())
            .map(|i| mock_call(&format!("call-{i}")))
            .collect();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();
        record_tool_results(&mut engine, &calls, results, None, &mut tracker).await;

        let ctx = engine.assemble("").await.unwrap();
        let stored: Vec<&String> = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(stored.len(), expected.len());
        for (content, want) in stored.iter().zip(&expected) {
            assert_eq!(classify_failure(content), *want, "content: {content}");
        }

        // REPEAT_ERROR is pushed verbatim by run_turn, not through
        // record_tool_results; pin it against the classifier too.
        assert_eq!(
            classify_failure(REPEAT_ERROR),
            Some(FailureKind::RepeatBlock),
        );
    }

    #[tokio::test]
    async fn test_activity_tool_events() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls(&["call-1", "call-2"])),
            Ok(text("Done")),
        ]));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(64);

        run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Activity test",
            &*provider,
            &tools,
            MAX_ITER,
            &tx_ctx(&tx),
        )
        .await
        .unwrap();

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[1], Activity::ToolStart { tool } if tool == "mock"));
        assert!(matches!(&events[2], Activity::ToolEnd { tool, error: None } if tool == "mock"));
        assert!(matches!(&events[3], Activity::ToolEnd { tool, error: None } if tool == "mock"));
    }

    #[tokio::test]
    async fn test_activity_max_iterations() {
        let responses = (0..MAX_ITER)
            .map(|n| Ok(mock_distinct_call(n)))
            .chain([Ok(text("state report"))])
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let tools = mock_tools("mock output");
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let (tx, mut rx) = mpsc::channel(256);

        let _ = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Max iter activity",
            &*provider,
            &tools,
            MAX_ITER,
            &tx_ctx(&tx),
        )
        .await;

        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert!(matches!(events.last().unwrap(), Activity::MaxIterations));
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_returns_cancelled() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let tools = Tools::default();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Should not run",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx {
                activity: None,
                cancel,
                ..ToolCtx::default()
            },
        )
        .await;
        assert!(matches!(result.unwrap_err(), Error::Cancelled));
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn test_process_message_saves_on_provider_error() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::workspace::Workspace::init_at(dir.path().to_path_buf()).unwrap();

        let provider = Arc::new(MockProvider::new(vec![Err(ProviderError::Network(
            "connection refused".into(),
        ))]));
        let tools = Tools::default();
        let summarize = test_summarize(&provider);

        let mut engine =
            FlatSession::new(&dir.path().join("context"), ContextConfig::default()).unwrap();

        let result = process_message_metered(
            &mut engine,
            &summarize,
            &workspace,
            "Hello?",
            &*provider,
            &tools,
            MAX_ITER,
            &PromptConfig {
                memory_index_cap: 8192,
                trusted_repos: Vec::new(),
            },
            false,
            &[],
            &ToolCtx::default(),
        )
        .await;
        let (result, _usage) = result;
        assert!(result.is_err());

        // The caller (actor) is responsible for saving. We verify the engine
        // recorded the user message.
        assert_eq!(engine.stats().message_count, 1);
    }

    /// The two modes are exclusive. A reviewer must not be told how to
    /// push and open a PR on the branch it is judging, and a builder
    /// must not be missing the workflow because a dispatch looked
    /// review-shaped.
    #[test]
    fn the_two_modes_do_not_share_segments() {
        let github = |role| ChannelSource::GitHub {
            pr_number: 1,
            repo: "owner/repo".into(),
            role,
        };
        let reviewer = role_segments(&github(GitHubRole::Reviewer));
        assert_eq!(
            reviewer,
            [crate::channel::github::prs::REVIEW_PROTOCOL_SEGMENT]
        );
        assert!(!reviewer.iter().any(|s| s.contains("## Developer Workflow")));

        // Author is the bot's own PR and Contributor is a third-party
        // PR it pushes fixes to; both are build work.
        for source in [
            github(GitHubRole::Author),
            github(GitHubRole::Contributor),
            ChannelSource::Duty {
                duty: "distill".into(),
            },
            ChannelSource::Socket,
            ChannelSource::Telegram,
        ] {
            let segments = role_segments(&source);
            assert_eq!(segments, [DEVELOPER_WORKFLOW], "{source}");
            assert!(
                !segments
                    .iter()
                    .any(|s| s.contains("Review this PR per the Review Protocol")),
                "{source} carries reviewer choreography"
            );
        }
    }

    /// The workflow left `AGENTS.md` to become a segment; the static
    /// prompt must not still carry a copy.
    #[test]
    fn the_static_prompt_no_longer_carries_the_workflow() {
        let agents = include_str!("../prompts/AGENTS.md");
        assert!(!agents.contains("## Developer Workflow"));
        assert!(DEVELOPER_WORKFLOW.starts_with("## Developer Workflow"));
        // Shared sections stayed behind.
        for kept in [
            "## Delegation",
            "## Guidelines",
            "## Memory",
            "## When Tools Fail",
        ] {
            assert!(agents.contains(kept), "AGENTS.md lost {kept}");
        }
    }

    /// Externalization is a property of the context engine, not of a
    /// mode, so it belongs in the always-present prompt. It lived only in
    /// the review protocol until that path stopped needing it, and a
    /// builder turn then met a `<file>` reference with no idea what it
    /// was and burned its whole budget re-reading the same file.
    #[test]
    fn the_static_prompt_explains_externalized_output() {
        let agents = include_str!("../prompts/AGENTS.md");
        for needle in ["<file>", "lcm_grep", "Never re-issue an identical call"] {
            assert!(agents.contains(needle), "AGENTS.md omits {needle}");
        }
    }

    // ── Policy violation gate ─────────────────────────────────────────

    fn blocked_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock_blocked".parse().unwrap(),
                arguments: "{}".to_string(),
            },
        )
    }

    fn blocked_tool_calls(ids: &[&str]) -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: ids.iter().map(|&id| blocked_call(id)).collect(),
        }
    }

    fn blocked_tools() -> Tools {
        Tools::new(vec![Arc::new(MockBlockedTool::new("not allowed"))], &[]).unwrap()
    }

    #[tokio::test]
    async fn test_first_blocked_injects_system_directive() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(text("OK I'll stop")),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Try blocked",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "OK I'll stop");

        let ctx = engine.assemble("").await.unwrap();
        let has_directive = ctx.messages.iter().any(
            |m| matches!(m, Message::System { content } if content.contains("POLICY VIOLATION")),
        );
        assert!(has_directive, "expected POLICY VIOLATION system message");
    }

    #[tokio::test]
    async fn test_second_blocked_halts_turn() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(blocked_tool_calls(&["b2"])),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Keep trying",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;

        let out = result.unwrap();
        assert!(
            matches!(out, TurnOutput::PolicyHalt { .. }),
            "expected policy halt outcome",
        );
        let msg = out.into_text();
        assert!(
            msg.contains("halted automatically"),
            "expected halt message, got: {msg}",
        );
        assert!(
            msg.contains("not allowed"),
            "expected blocked reason in halt message, got: {msg}",
        );
    }

    #[tokio::test]
    async fn distinct_rules_do_not_halt_the_turn() {
        // Two first offenses against different rules (issue #7 smoke
        // test: absolute working_dir, then deny-listed git fetch) each
        // get a directive; only repeating one rule halts.
        let call = |name: &str, id: &str| {
            ToolCall::new(
                id.to_string(),
                ToolFunction {
                    name: name.parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )
        };
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::ToolCalls {
                content: String::new(),
                calls: vec![call("rule_a", "b1")],
            }),
            Ok(Response::ToolCalls {
                content: String::new(),
                calls: vec![call("rule_b", "b2")],
            }),
            Ok(text("Reporting to the user")),
        ]));
        let tools = Tools::new(
            vec![
                Arc::new(MockBlockedTool::named("rule_a", "use the a tool")),
                Arc::new(MockBlockedTool::named("rule_b", "use the b tool")),
            ],
            &[],
        )
        .unwrap();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Two different mistakes",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "Reporting to the user");
    }

    #[tokio::test]
    async fn cross_rule_probing_halts_at_the_round_cap() {
        // Four rounds, four different rules: no rule repeats, but a
        // turn that keeps finding new walls is probing the guardrails.
        let call = |name: &str, id: &str| {
            ToolCall::new(
                id.to_string(),
                ToolFunction {
                    name: name.parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )
        };
        let round = |name: &str, id: &str| {
            Ok(Response::ToolCalls {
                content: String::new(),
                calls: vec![call(name, id)],
            })
        };
        let provider = Arc::new(MockProvider::new(vec![
            round("rule_a", "b1"),
            round("rule_b", "b2"),
            round("rule_c", "b3"),
            round("rule_d", "b4"),
            Ok(text("never reached")),
        ]));
        let tools = Tools::new(
            vec![
                Arc::new(MockBlockedTool::named("rule_a", "use a")),
                Arc::new(MockBlockedTool::named("rule_b", "use b")),
                Arc::new(MockBlockedTool::named("rule_c", "use c")),
                Arc::new(MockBlockedTool::named("rule_d", "use d")),
            ],
            &[],
        )
        .unwrap();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Probe everything",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert!(
            matches!(result.unwrap(), TurnOutput::PolicyHalt { .. }),
            "expected policy halt at the round cap",
        );
    }

    #[tokio::test]
    async fn parallel_blocks_of_one_rule_count_once() {
        // Both calls were issued before the directive could land, so
        // the round is one strike, not a halt.
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1", "b2"])),
            Ok(text("OK I'll stop")),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let result = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Parallel blocked calls",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(result.unwrap().into_text(), "OK I'll stop");
    }

    #[tokio::test]
    async fn test_policy_strikes_reset_between_turns() {
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_tool_calls(&["b1"])),
            Ok(text("Turn 1 done")),
            Ok(blocked_tool_calls(&["b2"])),
            Ok(text("Turn 2 done")),
        ]));
        let tools = blocked_tools();
        let mut engine = test_engine();
        let summarize = test_summarize(&provider);

        let r1 = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Turn 1",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(r1.unwrap().into_text(), "Turn 1 done");

        let r2 = run_turn(
            &mut engine,
            &summarize,
            SYSTEM,
            "Turn 2",
            &*provider,
            &tools,
            MAX_ITER,
            &ToolCtx::default(),
        )
        .await;
        assert_eq!(r2.unwrap().into_text(), "Turn 2 done");
    }

    // --- Tool strike escalation tests (issue #45) ---

    /// Helper: execute a tool and collect the result, the way the agent
    /// loop would for a single call.
    async fn exec_failing(
        tool: &Arc<dyn Tool>,
        call: &ToolCall,
        ctx: &ToolCtx,
    ) -> Result<String, ToolError> {
        let args: serde_json::Value =
            serde_json::from_str(&call.function.arguments).unwrap_or_default();
        tool.execute(args, ctx.clone()).await
    }

    /// Reproduce the 2026-08-13 turn-0 pattern: a tool that always
    /// fails with the same error, called repeatedly. By `STRIKE_NOTICE`
    /// (3) the error text names the repetition count; by `STRIKE_HALT`
    /// (5) the turn halts with `ToolHalt`.
    #[tokio::test]
    async fn tool_strike_escalates_identical_failures() {
        let tool = Arc::new(MockFailingTool::named("ci_status", || {
            ToolError::HttpStatus {
                url: "https://github.com/.../logs".into(),
                status: 502,
            }
        })) as Arc<dyn Tool>;
        let ctx = ToolCtx::default();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();
        let mut halt = None;

        for i in 0..STRIKE_HALT {
            let call = ToolCall {
                id: format!("call-{i}"),
                function: ToolFunction {
                    name: "ci_status".parse().unwrap(),
                    arguments: r#"{"repo":"owner/repo"}"#.into(),
                },
            };
            let result = exec_failing(&tool, &call, &ctx).await;
            let calls = vec![call];
            if let Some(h) =
                record_tool_results(&mut engine, &calls, vec![result], None, &mut tracker).await
            {
                halt = Some(h);
                break;
            }
        }

        match halt.expect("should halt by STRIKE_HALT") {
            TurnOutput::ToolHalt {
                tool,
                error_class,
                count,
                ..
            } => {
                assert_eq!(tool, "ci_status");
                assert_eq!(count, STRIKE_HALT);
                assert_eq!(error_class, "http_status:502");
            }
            other => panic!("expected ToolHalt, got {other:?}"),
        }
    }

    /// The error text at `STRIKE_NOTICE` contains the repetition count.
    #[tokio::test]
    async fn tool_strike_notice_at_threshold() {
        let tool = Arc::new(MockFailingTool::named("failing_tool", || {
            ToolError::Precondition("always fails".into())
        })) as Arc<dyn Tool>;
        let ctx = ToolCtx::default();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();

        for i in 0..STRIKE_NOTICE {
            let call = ToolCall {
                id: format!("call-{i}"),
                function: ToolFunction {
                    name: "failing_tool".parse().unwrap(),
                    arguments: r#"{"key":"value"}"#.into(),
                },
            };
            let result = exec_failing(&tool, &call, &ctx).await;
            let calls = vec![call];
            record_tool_results(&mut engine, &calls, vec![result], None, &mut tracker).await;
        }

        let ctx = engine.assemble("").await.unwrap();
        let last_tool_msg = ctx
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Tool { content, .. } => Some(content),
                _ => None,
            })
            .expect("should have a tool message");
        assert!(
            last_tool_msg.contains("failed 3 times this turn"),
            "expected repetition notice, got: {last_tool_msg}"
        );
    }

    /// Timeouts get progress-preserving guidance, not the deterministic
    /// escalation text. At `STRIKE_NOTICE` the guidance adds the caveat
    /// about store mtimes not advancing.
    #[tokio::test]
    async fn tool_strike_timeout_gets_progress_notice() {
        let tool = Arc::new(MockFailingTool::named("exec", || ToolError::Timeout {
            command: "cargo build".into(),
            secs: 600,
            evidence: crate::error::TimeoutEvidence::default(),
        })) as Arc<dyn Tool>;
        let ctx = ToolCtx::default();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();

        for i in 0..STRIKE_NOTICE {
            let call = ToolCall {
                id: format!("call-{i}"),
                function: ToolFunction {
                    name: "exec".parse().unwrap(),
                    arguments: r#"{"command":"cargo build"}"#.into(),
                },
            };
            let result = exec_failing(&tool, &call, &ctx).await;
            let calls = vec![call];
            record_tool_results(&mut engine, &calls, vec![result], None, &mut tracker).await;
        }

        let ctx = engine.assemble("").await.unwrap();
        let last_tool_msg = ctx
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Tool { content, .. } => Some(content),
                _ => None,
            })
            .expect("should have a tool message");
        assert!(
            last_tool_msg.contains("timed out 3 times this turn"),
            "expected timeout repetition notice, got: {last_tool_msg}"
        );
        assert!(
            last_tool_msg.contains("Partial progress persists"),
            "expected progress-preserving guidance, got: {last_tool_msg}"
        );
    }

    /// Different error classes from the same tool+args do NOT
    /// escalate: only identical (tool, args, `error_class`) signatures
    /// count. Alternating exit codes 1 and 2 produce different
    /// `command_failed:N` classes, so neither reaches `STRIKE_HALT`.
    #[tokio::test]
    async fn tool_strike_different_errors_dont_escalate() {
        let tool_a = Arc::new(MockFailingTool::named("exec", || {
            ToolError::CommandFailed {
                command: "cmd".into(),
                exit_code: 1,
                output: "fail".into(),
            }
        })) as Arc<dyn Tool>;
        let tool_b = Arc::new(MockFailingTool::named("exec", || {
            ToolError::CommandFailed {
                command: "cmd".into(),
                exit_code: 2,
                output: "fail".into(),
            }
        })) as Arc<dyn Tool>;
        let ctx = ToolCtx::default();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();
        let mut halt = None;

        for i in 0..((STRIKE_HALT - 1) * 2) {
            let t = if i % 2 == 0 { &tool_a } else { &tool_b };
            let call = ToolCall {
                id: format!("call-{i}"),
                function: ToolFunction {
                    name: "exec".parse().unwrap(),
                    arguments: r#"{"command":"cmd"}"#.into(),
                },
            };
            let result = exec_failing(t, &call, &ctx).await;
            let calls = vec![call];
            if let Some(h) =
                record_tool_results(&mut engine, &calls, vec![result], None, &mut tracker).await
            {
                halt = Some(h);
                break;
            }
        }

        assert!(
            halt.is_none(),
            "different error classes should not escalate to halt"
        );
    }

    /// Blocked errors are excluded from strike tracking — already
    /// handled by the policy strike system.
    #[tokio::test]
    async fn tool_strike_excludes_blocked_errors() {
        let tool = Arc::new(MockBlockedTool::named(
            "blocked_tool",
            "you are not allowed",
        )) as Arc<dyn Tool>;
        let ctx = ToolCtx::default();

        let mut engine = test_engine();
        let mut tracker = ToolStrikeTracker::default();
        let mut halt = None;

        for i in 0..(STRIKE_HALT + 2) {
            let call = ToolCall {
                id: format!("call-{i}"),
                function: ToolFunction {
                    name: "blocked_tool".parse().unwrap(),
                    arguments: r"{}".into(),
                },
            };
            let result = exec_failing(&tool, &call, &ctx).await;
            let calls = vec![call];
            if let Some(h) =
                record_tool_results(&mut engine, &calls, vec![result], None, &mut tracker).await
            {
                halt = Some(h);
                break;
            }
        }

        assert!(halt.is_none(), "Blocked errors should not trigger ToolHalt");
        assert!(
            tracker.strikes.is_empty(),
            "Blocked errors should not be recorded in strike tracker"
        );
    }

    /// `canonical_args` normalizes JSON key order so semantically
    /// identical arguments match.
    #[test]
    fn canonical_args_normalizes_key_order() {
        let a = canonical_args(r#"{"repo":"owner/repo","branch":"main"}"#);
        let b = canonical_args(r#"{"branch":"main","repo":"owner/repo"}"#);
        assert_eq!(a, b);
    }
}
