//! The `task` tool: delegated sub-agent turns.
//!
//! See `specs/19-sub-agents.md` for the design. A `task` call runs the
//! same `run_turn` loop the parent uses, against a fresh
//! [`EphemeralSession`], with a per-type tool allowlist. The child's
//! tool calls and reasoning stay in its own context; the parent sees
//! only the final assistant text.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Instrument, warn};

use crate::activity::Activity;
use crate::context::SummarizeFn;
use crate::context::ephemeral::EphemeralSession;
use crate::error::ToolError;
use crate::provider::Provider;
use crate::review::{self, ReviewLedger};
use crate::tools::mcp::McpTools;
use crate::tools::{Tool, ToolCtx, Tools, string_or_value};
use crate::usage::{self, TurnRecord, UsageLedger};

use super::{BudgetPolicy, ReplyPolicy, TurnMeter, run_turn_metered};

/// Allowlist for the `explore` type: read-only research.
///
/// Names absent from the registry (disabled or compiled out) are
/// skipped by `Tools::filtered`; typos here are caught by
/// `allowlists_reference_real_tools`.
const EXPLORE_TOOLS: &[&str] = &[
    "file_read",
    "glob_search",
    "grep",
    "web_fetch",
    "web_search",
];

/// Allowlist for the `worker` type: explore plus write and exec.
///
/// Deliberately no git/GitHub tools — outward-visible actions stay
/// with the parent (spec 19).
const WORKER_TOOLS: &[&str] = &[
    "file_read",
    "glob_search",
    "grep",
    "web_fetch",
    "web_search",
    "file_write",
    "file_edit",
    "exec",
];

/// Allowlist for the `reviewer` type: explore's tools, and — unlike
/// explore — no LCM tools either (see `build_agent_types`). Web stays
/// because verifying a suspected hallucinated API against real
/// documentation is a core review move (spec 23).
const REVIEWER_TOOLS: &[&str] = &[
    "file_read",
    "glob_search",
    "grep",
    "web_fetch",
    "web_search",
];

/// Tool output cap (estimated tokens) for sub-agent contexts.
///
/// Deliberately far above the root's `context.tool_output_tokens`:
/// sub-agents exist to absorb verbose output, and `lcm_expand` may
/// legitimately return up to `MAX_EXPAND_TOKEN_CAP` (20k) in one tool
/// result. Matching it means expansion output survives untruncated.
const SUB_AGENT_TOOL_OUTPUT_TOKENS: usize = 20_000;

const EXPLORE_PROMPT: &str = include_str!("../prompts/explore.md");

const WORKER_PROMPT: &str = include_str!("../prompts/worker.md");

const REVIEWER_PROMPT: &str = include_str!("../prompts/reviewer.md");

/// The corrective turn after an unparseable findings block: the
/// reviewer sees its own prior text in the session and re-emits the
/// block; one attempt, no new analysis.
const FINDINGS_RETRY_PROMPT: &str = "Your previous response had no parseable ```findings \
block (exactly one fenced block holding a single JSON object). Re-emit your review's \
verdict and findings as exactly one such block, unchanged in substance, nothing else.";

/// A sub-agent type: system prompt plus prebuilt tool set.
pub(crate) struct AgentType {
    system_prompt: String,
    tools: Tools,
}

/// The three prebuilt sub-agent types.
pub(crate) struct AgentTypes {
    pub explore: AgentType,
    pub worker: AgentType,
    pub reviewer: AgentType,
}

/// Per-type providers, resolved from `provider.model_overrides` at
/// startup.
pub(crate) struct TypeProviders<P> {
    pub explore: Arc<P>,
    pub worker: Arc<P>,
    pub reviewer: Arc<P>,
}

/// Build the sub-agent types from the parent's base registry
/// (post-`tools.disabled`), the engine's sub-agent tools, and the MCP
/// registrations. Explore and worker share the engine tool instances;
/// the reviewer gets none — its independence from the parent's
/// narrative is the design point, and LCM tools would hand that
/// narrative back (spec 23). The worker takes every MCP tool (it
/// already holds exec; nothing a server advertises is riskier); the
/// read-only types take only servers whose config asserts no side
/// effects (spec 22). No child can see `task`, so recursion is
/// structurally impossible.
pub(crate) fn build_agent_types(
    base: &Tools,
    engine_tools: Vec<Arc<dyn Tool>>,
    mcp: &McpTools,
    workspace_dir: &Path,
    max_iterations: usize,
) -> AgentTypes {
    let mut explore_tools = base.filtered(EXPLORE_TOOLS);
    explore_tools.extend_with(engine_tools.clone(), &[]);
    explore_tools.extend_with(mcp.explore.clone(), &[]);
    let mut worker_tools = base.filtered(WORKER_TOOLS);
    worker_tools.extend_with(engine_tools, &[]);
    worker_tools.extend_with(mcp.all.clone(), &[]);
    let mut reviewer_tools = base.filtered(REVIEWER_TOOLS);
    reviewer_tools.extend_with(mcp.explore.clone(), &[]);

    AgentTypes {
        explore: AgentType {
            system_prompt: compose_prompt(
                EXPLORE_PROMPT,
                workspace_dir,
                &explore_tools,
                max_iterations,
            ),
            tools: explore_tools,
        },
        worker: AgentType {
            system_prompt: compose_prompt(
                WORKER_PROMPT,
                workspace_dir,
                &worker_tools,
                max_iterations,
            ),
            tools: worker_tools,
        },
        reviewer: AgentType {
            system_prompt: compose_prompt(
                REVIEWER_PROMPT,
                workspace_dir,
                &reviewer_tools,
                max_iterations,
            ),
            tools: reviewer_tools,
        },
    }
}

/// Append environment info (working directory, iteration budget, tool
/// names) to a type's role prompt, mirroring the parent's system
/// prompt assembly.
fn compose_prompt(
    role: &str,
    workspace_dir: &Path,
    tools: &Tools,
    max_iterations: usize,
) -> String {
    let names: Vec<String> = tools
        .definitions()
        .into_iter()
        .map(|d| d.function.name.to_string())
        .collect();
    format!(
        "{}\n\n# Environment\nWorking directory: {}\nRepository checkouts live at projects/<owner>/<repo> (work) or \
        reviews/<owner>/<repo> (a read-only worktree at a pull request's head); \
        resolve repo-relative paths against the checkout root named in your \
        task.\nIteration budget: {} tool rounds; parallel tool calls within a \
        round count once.\nAvailable tools: {}",
        role.trim_end(),
        workspace_dir.display(),
        max_iterations,
        names.join(", "),
    )
}

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AgentKind {
    #[default]
    Explore,
    Worker,
    Reviewer,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// The task for the sub-agent to perform. The sub-agent cannot see
    /// the conversation history; include all necessary context.
    prompt: String,
    /// "explore" (default): read-only research. "worker": can also
    /// write files and execute commands. "reviewer": read-only judge
    /// for a packed artifact.
    #[serde(default)]
    agent_type: AgentKind,
    /// Reviewer calls only: ledger context for the gate invocation.
    /// Ignored for other agent types.
    #[serde(default, deserialize_with = "string_or_value")]
    review: Option<ReviewMeta>,
}

/// Where a review lands in the ledger. Supplied by the parent when it
/// dispatches a reviewer; the recording itself is mechanical.
#[derive(Deserialize, JsonSchema)]
struct ReviewMeta {
    /// Repository under review, `owner/repo`.
    repo: String,
    /// Which gate: "plan", "commit", "series", or "pr".
    gate: String,
    /// The ref under review: SHA for commit/series/pr, branch for plan.
    git_ref: String,
}

/// Tool that runs a sub-agent turn in an isolated in-memory context.
///
/// Generic over the provider because [`Provider`] is not object-safe;
/// the registry erases the generic behind `Arc<dyn Tool>`.
pub(crate) struct TaskTool<P: Provider> {
    providers: TypeProviders<P>,
    summarize: SummarizeFn,
    types: AgentTypes,
    max_iterations: usize,
    usage_ledger: Option<Arc<UsageLedger>>,
    review_ledger: Option<Arc<ReviewLedger>>,
}

impl<P: Provider> TaskTool<P> {
    pub fn new(
        providers: TypeProviders<P>,
        summarize: SummarizeFn,
        types: AgentTypes,
        max_iterations: usize,
        usage_ledger: Option<Arc<UsageLedger>>,
        review_ledger: Option<Arc<ReviewLedger>>,
    ) -> Self {
        Self {
            providers,
            summarize,
            types,
            max_iterations,
            usage_ledger,
            review_ledger,
        }
    }
}

impl<P: Provider> Tool for TaskTool<P> {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Launch a sub-agent to perform a task in an isolated context. The \
        sub-agent runs independently and returns its findings as text. Use \
        this for:\n\
        - Searching the codebase for specific patterns or information\n\
        - Reading and analyzing files without polluting your context\n\
        - Performing self-contained tasks that produce verbose intermediate \
        output\n\
        - Expanding compacted history via lcm_expand (only available to \
        sub-agents)\n\n\
        The sub-agent cannot see your conversation history. Pack all \
        necessary context into the prompt.\n\n\
        agent_type \"explore\" (default): read-only research. Tools: \
        file_read, glob_search, grep, web_fetch, web_search. No exec, git, \
        or GitHub: it cannot fetch PRs, clone, or run commands. Hand it \
        files that already exist in the workspace and questions about \
        them, not fetch jobs.\n\
        agent_type \"worker\": explore's tools plus file_write, file_edit, \
        and exec. For self-contained mechanical tasks. No git or GitHub \
        tools.\n\
        agent_type \"reviewer\": read-only judge for an artifact you pack \
        into the prompt (a plan, a staged diff with its commit message, a \
        branch diff, or a pull request diff) plus its stated intent. \
        Explore's tools but no access to compacted history. Returns prose \
        findings ending in a fenced findings block."
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            let (agent, provider, label) = match args.agent_type {
                AgentKind::Explore => (&self.types.explore, &self.providers.explore, "explore"),
                AgentKind::Worker => (&self.types.worker, &self.providers.worker, "worker"),
                AgentKind::Reviewer => (&self.types.reviewer, &self.providers.reviewer, "reviewer"),
            };

            // Fresh context per call, discarded on return. The parent's
            // token is threaded through so the child can observe
            // cancellation at an iteration boundary, though the primary
            // cancel path is still the parent dropping this future.
            let mut engine = EphemeralSession::new(SUB_AGENT_TOOL_OUTPUT_TOKENS);
            let child_ctx = ToolCtx {
                activity: ctx.activity.as_ref().map(|parent| forward(parent, label)),
                cancel: ctx.cancel.clone(),
                task: ctx.task.clone(),
                tool_output_tokens: Some(SUB_AGENT_TOOL_OUTPUT_TOKENS),
            };
            // FinalAnswer: a maxed-out sub-agent returns a degraded
            // answer instead of erroring, so a reviewer that runs out
            // of budget still delivers a verdict and its cost is
            // recorded.
            let (result, meter) = run_turn_metered(
                &mut engine,
                &self.summarize,
                &agent.system_prompt,
                &args.prompt,
                &**provider,
                &agent.tools,
                self.max_iterations,
                BudgetPolicy::FinalAnswer,
                ReplyPolicy::Accept,
                &child_ctx,
            )
            .instrument(tracing::info_span!("subagent", agent = label))
            .await;
            // Bill before propagating: a sub-agent that errored still
            // spent its calls. The reviewer branch's retry spend is
            // already folded into the meter, so one row bills it all.
            let output = match result {
                Ok(output) => output,
                Err(e) => {
                    usage::record_turn(
                        self.usage_ledger.as_deref(),
                        &TurnRecord {
                            // Inherited from the parent: sub-agent spend
                            // folds into the task that delegated it
                            // (spec 27).
                            task: ctx.task.as_ref(),
                            session: "subagent",
                            source: label,
                            model: provider.model(),
                            meter,
                        },
                    );
                    return Err(ToolError::SubAgent {
                        source: Box::new(e),
                    });
                }
            };
            let mut text = output.into_text();
            let mut meter = meter;
            if args.agent_type == AgentKind::Reviewer {
                let (recorded, retry_meter) = self
                    .record_review_with_retry(
                        ReviewRetry {
                            engine: &mut engine,
                            meta: args.review.as_ref(),
                            provider,
                            system_prompt: &agent.system_prompt,
                            tools: &agent.tools,
                            max_iterations: self.max_iterations,
                            ctx: &child_ctx,
                        },
                        &text,
                    )
                    .instrument(tracing::info_span!("subagent", agent = "reviewer-retry"))
                    .await;
                append_recording_trailer(&mut text, &recorded);
                // The retry's spend folds into this gate invocation's
                // single usage row (spec 27).
                if let Some(retry) = retry_meter {
                    meter.usage.add_turn(&retry.usage);
                    meter.duration += retry.duration;
                }
            }
            usage::record_turn(
                self.usage_ledger.as_deref(),
                &TurnRecord {
                    // Inherited from the parent: sub-agent spend folds
                    // into the task that delegated it (spec 27).
                    task: ctx.task.as_ref(),
                    session: "subagent",
                    source: label,
                    model: provider.model(),
                    meter,
                },
            );
            Ok(text)
        })
    }
}

/// How a reviewer's findings block landed in the ledger. `Disabled` is
/// a ledger-off no-op, `Recorded` an empty vec on a clean review.
enum ReviewRecording {
    Disabled,
    Recorded(Vec<i64>),
    Unparseable,
    WriteFailed,
}

/// Everything the corrective retry turn needs, borrowed from the
/// reviewer invocation.
struct ReviewRetry<'a, P: Provider> {
    engine: &'a mut EphemeralSession,
    meta: Option<&'a ReviewMeta>,
    provider: &'a P,
    system_prompt: &'a str,
    tools: &'a Tools,
    max_iterations: usize,
    ctx: &'a ToolCtx,
}

/// A failure trailer means nothing was recorded: the parent must not
/// cite ids or call `review_disposition` for them. A clean review
/// (empty `Recorded`) and `Disabled` get no trailer at all.
fn append_recording_trailer(text: &mut String, recorded: &ReviewRecording) {
    let trailer = match recorded {
        ReviewRecording::Recorded(ids) if !ids.is_empty() => {
            let list = ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!("\n\n[ledger: finding ids {list}]")
        }
        ReviewRecording::Unparseable => {
            "\n\n[ledger: findings block unparseable; nothing recorded]".to_string()
        }
        ReviewRecording::WriteFailed => {
            "\n\n[ledger: recording failed; nothing recorded]".to_string()
        }
        ReviewRecording::Recorded(_) | ReviewRecording::Disabled => return,
    };
    let _ = write!(text, "{trailer}");
}

impl<P: Provider> TaskTool<P> {
    /// Parse a reviewer response's findings block and record it —
    /// mechanically, no model cooperation required. Telemetry only:
    /// every failure is a warning, never a review failure. An
    /// unparseable block gets one corrective turn on the same session
    /// (the reviewer sees its own text) before giving up; that turn's
    /// meter comes back for folding into the invocation's usage row.
    async fn record_review_with_retry(
        &self,
        retry: ReviewRetry<'_, P>,
        text: &str,
    ) -> (ReviewRecording, Option<TurnMeter>) {
        let Some(ledger) = self.review_ledger.as_deref() else {
            return (ReviewRecording::Disabled, None);
        };
        match record_parse(ledger, retry.meta, text) {
            recording @ (ReviewRecording::Recorded(_) | ReviewRecording::WriteFailed) => {
                (recording, None)
            }
            ReviewRecording::Unparseable => {
                warn!("reviewer response has no parseable findings block; retrying once");
                let ReviewRetry {
                    engine,
                    provider,
                    system_prompt,
                    tools,
                    max_iterations,
                    ctx,
                    ..
                } = retry;
                let (result, meter) = run_turn_metered(
                    engine,
                    &self.summarize,
                    system_prompt,
                    FINDINGS_RETRY_PROMPT,
                    provider,
                    tools,
                    max_iterations,
                    BudgetPolicy::FinalAnswer,
                    ReplyPolicy::Accept,
                    ctx,
                )
                .await;
                match result {
                    Ok(output) => {
                        let retry_text = output.into_text();
                        (record_parse(ledger, retry.meta, &retry_text), Some(meter))
                    }
                    Err(e) => {
                        warn!("findings retry turn failed: {e}");
                        (ReviewRecording::Unparseable, Some(meter))
                    }
                }
            }
            ReviewRecording::Disabled => (ReviewRecording::Disabled, None),
        }
    }
}

/// Parse `text`'s findings block and write it to the ledger.
fn record_parse(ledger: &ReviewLedger, meta: Option<&ReviewMeta>, text: &str) -> ReviewRecording {
    let Some(output) = review::parse_findings_block(text) else {
        return ReviewRecording::Unparseable;
    };
    // A missing meta mislabels the rows but must not drop them: the
    // category counts are the point.
    let (repo, gate, git_ref) = meta.map_or(("", "", ""), |m| {
        (m.repo.as_str(), m.gate.as_str(), m.git_ref.as_str())
    });
    if meta.is_none() {
        warn!("reviewer call carried no review metadata; recording unlabeled");
    }
    let record = review::GateRecord {
        repo,
        gate,
        git_ref,
    };
    match ledger.record_review(&record, &output) {
        Ok(ids) => ReviewRecording::Recorded(ids),
        Err(e) => {
            warn!("failed to record review: {e}");
            ReviewRecording::WriteFailed
        }
    }
}

/// Spawn a forwarder that wraps child activity events in
/// [`Activity::Nested`] labeled with the agent type, and relays them
/// to the parent sink.
///
/// The forwarder ends when the returned sender (held only by the child
/// ctx) is dropped at the end of the task call.
fn forward(parent: &mpsc::Sender<Activity>, agent: &'static str) -> mpsc::Sender<Activity> {
    let parent = parent.clone();
    let (tx, mut rx) = mpsc::channel(32);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let _ = parent.try_send(Activity::Nested {
                agent: agent.to_string(),
                event: Box::new(event),
            });
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::tools::{DirenvCache, MockTool};
    use crate::types::{Response, ToolCall, ToolFunction};
    use crate::workspace::Workspace;

    fn noop_summarize() -> SummarizeFn {
        Arc::new(|_prompt: &str, _messages: &[crate::types::Message]| {
            Box::pin(async { Ok(String::new()) })
                as Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>
        })
    }

    fn mock_tool_calls() -> Response {
        Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c1".to_string(),
                ToolFunction {
                    name: "mock".parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )],
        }
    }

    fn agent_type(tools: Tools) -> AgentType {
        AgentType {
            system_prompt: "test agent".to_string(),
            tools,
        }
    }

    fn same_provider(provider: &Arc<MockProvider>) -> TypeProviders<MockProvider> {
        TypeProviders {
            explore: provider.clone(),
            worker: provider.clone(),
            reviewer: provider.clone(),
        }
    }

    fn task_tool(
        responses: Vec<Result<Response, ProviderError>>,
        explore: Tools,
        worker: Tools,
        max_iterations: usize,
    ) -> TaskTool<MockProvider> {
        let provider = Arc::new(MockProvider::new(responses));
        TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(explore),
                worker: agent_type(worker),
                reviewer: agent_type(Tools::default()),
            },
            max_iterations,
            None,
            None,
        )
    }

    fn mock_tools() -> Tools {
        Tools::new(vec![Arc::new(MockTool::new("mock output"))], &[]).unwrap()
    }

    /// Sub-agent rows inherit the parent's task key through the ctx,
    /// so delegated spend folds into the delegating task (spec 27).
    #[tokio::test]
    async fn child_row_inherits_the_parent_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap();
        let ledger = Arc::new(UsageLedger::new(&db, std::collections::HashMap::new()));
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text("done".into()))]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            Some(ledger.clone()),
            None,
        );

        let task = usage::TaskKey::for_source(&crate::agent::envelope::ChannelSource::Duty {
            duty: "self-analysis".into(),
        });
        tool.execute(
            serde_json::json!({"prompt": "look around"}),
            ToolCtx {
                task: Some(task),
                ..ToolCtx::default()
            },
        )
        .await
        .unwrap();

        let rows: Vec<(String, String, Option<String>)> = {
            let conn = db.connection();
            let conn = conn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut stmt = conn
                .prepare("SELECT session, source, task FROM turns")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(rows.len(), 1);
        let (session, source, stored) = &rows[0];
        assert_eq!(session, "subagent");
        assert_eq!(source, "explore");
        assert_eq!(stored.as_deref(), Some("duty:self-analysis"));
    }

    /// No dispatch identity, no task: a default ctx records NULL.
    #[tokio::test]
    async fn child_row_without_task_records_null() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap();
        let ledger = Arc::new(UsageLedger::new(&db, std::collections::HashMap::new()));
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text("done".into()))]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            Some(ledger),
            None,
        );

        tool.execute(serde_json::json!({"prompt": "look"}), ToolCtx::default())
            .await
            .unwrap();

        let conn = db.connection();
        let conn = conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored: Option<String> = conn
            .query_row("SELECT task FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
    }

    /// Full local tool catalog plus the cfg-gated network names.
    fn catalog_and_types() -> (Vec<&'static str>, AgentTypes) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let config = crate::config::Config::load(dir.path()).unwrap();
        let local = Tools::local(&workspace, &config, DirenvCache::new());
        let mut names: Vec<&'static str> = local.iter().map(|t| t.name()).collect();
        // Network tools are cfg-gated out under mock-network; their
        // names are pinned by spec 03.
        names.extend(["web_fetch", "web_search"]);
        let base = Tools::new(local, &[]).unwrap();
        let types = build_agent_types(
            &base,
            Vec::new(),
            &McpTools::default(),
            workspace.path(),
            30,
        );
        (names, types)
    }

    fn tool_names(tools: &Tools) -> Vec<String> {
        tools
            .definitions()
            .into_iter()
            .map(|d| d.function.name.to_string())
            .collect()
    }

    #[tokio::test]
    async fn returns_child_final_text() {
        let tool = task_tool(
            vec![Ok(Response::Text("child says done".to_string()))],
            Tools::default(),
            Tools::default(),
            5,
        );
        let result = tool
            .execute(
                serde_json::json!({"prompt": "do a thing"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(result, "child says done");
    }

    #[tokio::test]
    async fn child_runs_tool_loop() {
        let tool = task_tool(
            vec![
                Ok(mock_tool_calls()),
                Ok(Response::Text("used the tool".to_string())),
            ],
            mock_tools(),
            Tools::default(),
            5,
        );
        let result = tool
            .execute(
                serde_json::json!({"prompt": "use a tool"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(result, "used the tool");
    }

    #[tokio::test]
    async fn worker_type_uses_worker_tools() {
        // Explore's set is empty: if the default type were picked, the
        // child's mock call would fail with NotFound and the provider
        // would see an error message instead of tool output.
        let provider = Arc::new(MockProvider::new(vec![
            Ok(mock_tool_calls()),
            Ok(Response::Text("worker done".to_string())),
        ]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(mock_tools()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            None,
        );
        let result = tool
            .execute(
                serde_json::json!({"prompt": "work", "agent_type": "worker"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(result, "worker done");
    }

    #[tokio::test]
    async fn each_agent_type_uses_its_own_provider() {
        let explore_provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "from explore provider".to_string(),
        ))]));
        let worker_provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "from worker provider".to_string(),
        ))]));
        let reviewer_provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "from reviewer provider".to_string(),
        ))]));
        let tool = TaskTool::new(
            TypeProviders {
                explore: explore_provider.clone(),
                worker: worker_provider.clone(),
                reviewer: reviewer_provider.clone(),
            },
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            None,
        );

        for (kind, expected) in [
            ("explore", "from explore provider"),
            ("worker", "from worker provider"),
            ("reviewer", "from reviewer provider"),
        ] {
            let result = tool
                .execute(
                    serde_json::json!({"prompt": "x", "agent_type": kind}),
                    ToolCtx::default(),
                )
                .await
                .unwrap();
            assert_eq!(result, expected);
        }

        assert_eq!(explore_provider.call_count(), 1);
        assert_eq!(worker_provider.call_count(), 1);
        assert_eq!(reviewer_provider.call_count(), 1);
    }

    #[tokio::test]
    async fn forwards_child_activity_labeled_with_agent_type() {
        let tool = task_tool(
            vec![
                Ok(mock_tool_calls()),
                Ok(Response::Text("used the tool".to_string())),
            ],
            Tools::default(),
            mock_tools(),
            5,
        );
        let (tx, mut rx) = mpsc::channel(64);
        tool.execute(
            serde_json::json!({"prompt": "use a tool", "agent_type": "worker"}),
            ToolCtx {
                activity: Some(tx),
                ..ToolCtx::default()
            },
        )
        .await
        .unwrap();
        // The parent sender is gone (moved into the ctx) and the
        // forwarder's clone drops when it exits, so recv() drains the
        // full stream and then terminates.
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        let nested: Vec<(&str, &Activity)> = events
            .iter()
            .filter_map(|e| match e {
                Activity::Nested { agent, event } => Some((agent.as_str(), event.as_ref())),
                _ => None,
            })
            .collect();
        assert!(nested.iter().any(|(agent, e)| *agent == "worker"
            && matches!(e, Activity::ToolStart { tool } if tool == "mock")));
        assert!(nested.iter().any(|(agent, e)| *agent == "worker"
            && matches!(e, Activity::ToolEnd { tool, error: None } if tool == "mock")));
    }

    #[tokio::test]
    async fn pre_cancelled_parent_token_stops_child() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "never".to_string(),
        ))]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            None,
        );
        let ctx = ToolCtx::default();
        ctx.cancel.cancel();
        let err = tool
            .execute(serde_json::json!({"prompt": "x"}), ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::SubAgent { source } => {
                assert!(matches!(*source, crate::error::Error::Cancelled));
            }
            other => panic!("expected SubAgent, got {other:?}"),
        }
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn max_iterations_squeezes_final_answer() {
        let max = 3;
        let mut responses = vec![Ok(mock_tool_calls()); max];
        responses.push(Ok(Response::Text("partial answer".to_string())));
        let tool = task_tool(responses, mock_tools(), Tools::default(), max);
        let result = tool
            .execute(
                serde_json::json!({"prompt": "loop forever"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(result, "partial answer");
    }

    #[tokio::test]
    async fn squeezed_reviewer_still_records_findings() {
        let max = 2;
        let mut responses = vec![Ok(mock_tool_calls()); max];
        responses.push(Ok(Response::Text(REVIEW_RESPONSE.to_string())));
        let provider = Arc::new(MockProvider::new(responses));
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            max,
            None,
            Some(ledger.clone()),
        );
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(result.contains("```findings"), "{result}");
        let report = ledger.report().unwrap();
        assert!(report.contains("swallowed-error"), "{report}");
    }

    #[tokio::test]
    async fn provider_error_surfaces_as_tool_error() {
        let tool = task_tool(
            vec![Err(ProviderError::RateLimited)],
            Tools::default(),
            Tools::default(),
            5,
        );
        let err = tool
            .execute(serde_json::json!({"prompt": "x"}), ToolCtx::default())
            .await
            .unwrap_err();
        // The provider's own error must survive the sub-agent boundary.
        assert!(matches!(
            &err,
            ToolError::SubAgent { source }
                if matches!(**source, crate::error::Error::Provider(_))
        ));
    }

    const REVIEW_RESPONSE: &str = "Looks broken.\n```findings\n\
        {\"verdict\": \"incorrect\", \"confidence\": 0.9, \
        \"explanation\": \"bug\", \"findings\": [\
        {\"category\": \"swallowed-error\", \"severity\": \"must-fix\", \
        \"note\": \"drops err\"}]}\n```";

    fn tool_with_review_ledger(ledger: Arc<crate::review::ReviewLedger>) -> TaskTool<MockProvider> {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            REVIEW_RESPONSE.to_string(),
        ))]));
        TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger),
        )
    }

    #[tokio::test]
    async fn reviewer_output_recorded_to_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let tool = tool_with_review_ledger(ledger.clone());
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        // The parent still gets the full text, block included, plus
        // the mechanical id trailer it needs for dispositions.
        assert!(result.contains("```findings"), "{result}");
        assert!(result.contains("[ledger: finding ids 1]"), "{result}");
        let report = ledger.report().unwrap();
        assert!(report.contains("commit"), "{report}");
        assert!(report.contains("swallowed-error"), "{report}");
    }

    /// The incident's real bad shape (issue #113): a dispatch prompt
    /// respec'd the block into plain verdict/findings prose.
    const BAD_SHAPE_RESPONSE: &str =
        "Looks broken.\nverdict: incorrect\nfindings:\n- id: 1\n  note: drops err\n";

    fn malformed_response_provider() -> Arc<MockProvider> {
        Arc::new(MockProvider::new(vec![
            Ok(Response::Text(BAD_SHAPE_RESPONSE.to_string())),
            Ok(Response::Text(REVIEW_RESPONSE.to_string())),
        ]))
    }

    #[tokio::test]
    async fn unparseable_block_retried_once_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let provider = malformed_response_provider();
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger.clone()),
        );
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        // Two calls: the review, then the corrective re-emit. The
        // parent sees the original prose plus the recorded ids.
        assert_eq!(provider.call_count(), 2, "{result}");
        assert!(
            result.contains("verdict: incorrect"),
            "original prose must reach the parent: {result}"
        );
        assert!(result.contains("[ledger: finding ids 1]"), "{result}");
        let report = ledger.report().unwrap();
        assert!(report.contains("swallowed-error"), "{report}");
    }

    #[tokio::test]
    async fn unparseable_after_retry_carries_the_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text(BAD_SHAPE_RESPONSE.to_string())),
            Ok(Response::Text(BAD_SHAPE_RESPONSE.to_string())),
        ]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger.clone()),
        );
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 2);
        assert!(
            result.contains("[ledger: findings block unparseable; nothing recorded]"),
            "{result}"
        );
        assert_eq!(ledger.report().unwrap(), "No reviews recorded.");
    }

    #[tokio::test]
    async fn failed_retry_turn_still_delivers_the_review() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text(BAD_SHAPE_RESPONSE.to_string())),
            Err(ProviderError::RateLimited),
        ]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger.clone()),
        );
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(
            result.contains("[ledger: findings block unparseable; nothing recorded]"),
            "a failed retry must not fail the task: {result}"
        );
        assert!(result.contains("verdict: incorrect"), "{result}");
        assert_eq!(ledger.report().unwrap(), "No reviews recorded.");
    }

    #[tokio::test]
    async fn disabled_ledger_makes_no_retry() {
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            BAD_SHAPE_RESPONSE.to_string(),
        ))]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            None,
        );
        let result = tool
            .execute(
                serde_json::json!({"prompt": "review this", "agent_type": "reviewer"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 1, "no retry without a ledger");
        assert!(!result.contains("[ledger:"), "{result}");
    }

    /// Exercise the ledger-write failure path: the reviews table is
    /// dropped behind the ledger's back, so `record_review` fails at
    /// the statement — the same `Err` any schema/IO failure surfaces.
    #[tokio::test]
    async fn failed_ledger_write_carries_its_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let review = "Bad.\n```findings\n\
             {\"verdict\": \"incorrect\", \"confidence\": 0.9, \
             \"explanation\": \"x\", \
             \"findings\": [{\"category\": \"c\", \"note\": \"n\"}]}\n```";
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            review.to_string(),
        ))]));
        {
            let conn = ledger.connection_for_test();
            conn.execute("DROP TABLE reviews", []).unwrap();
        }
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger.clone()),
        );
        let result = tool
            .execute(
                serde_json::json!({
                    "prompt": "review this",
                    "agent_type": "reviewer",
                    "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 1, "no retry on a write failure");
        assert!(
            result.contains("[ledger: recording failed; nothing recorded]"),
            "{result}"
        );
        assert!(
            !result.contains("[ledger: finding ids"),
            "a failed write must not cite ids: {result}"
        );
    }

    #[tokio::test]
    async fn clean_review_gets_no_id_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let clean = "All good.\n```findings\n{\"verdict\": \"correct\", \
                     \"confidence\": 1.0, \"explanation\": \"clean\", \
                     \"findings\": []}\n```";
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            clean.to_string(),
        ))]));
        let tool = TaskTool::new(
            same_provider(&provider),
            noop_summarize(),
            AgentTypes {
                explore: agent_type(Tools::default()),
                worker: agent_type(Tools::default()),
                reviewer: agent_type(Tools::default()),
            },
            5,
            None,
            Some(ledger),
        );
        let result = tool
            .execute(
                serde_json::json!({"prompt": "review this", "agent_type": "reviewer"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(!result.contains("[ledger:"), "{result}");
    }

    #[tokio::test]
    async fn non_reviewer_output_never_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let tool = tool_with_review_ledger(ledger.clone());
        // An explore response that happens to contain a findings block
        // must not land in the ledger, and must not grow a trailer.
        let result = tool
            .execute(
                serde_json::json!({"prompt": "research this"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(!result.contains("[ledger:"), "{result}");
        assert_eq!(ledger.report().unwrap(), "No reviews recorded.");
    }

    #[tokio::test]
    async fn reviewer_without_meta_still_records() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(crate::review::ReviewLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        ));
        let tool = tool_with_review_ledger(ledger.clone());
        tool.execute(
            serde_json::json!({"prompt": "review this", "agent_type": "reviewer"}),
            ToolCtx::default(),
        )
        .await
        .unwrap();
        let report = ledger.report().unwrap();
        assert!(report.contains("swallowed-error"), "{report}");
    }

    #[tokio::test]
    async fn invalid_agent_type_rejected() {
        let tool = task_tool(vec![], Tools::default(), Tools::default(), 5);
        let err = tool
            .execute(
                serde_json::json!({"prompt": "x", "agent_type": "manager"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn missing_prompt_rejected() {
        let tool = task_tool(vec![], Tools::default(), Tools::default(), 5);
        let err = tool
            .execute(serde_json::json!({}), ToolCtx::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn agent_type_defaults_to_explore() {
        let args: Args = serde_json::from_value(serde_json::json!({"prompt": "x"})).unwrap();
        assert_eq!(args.agent_type, AgentKind::Explore);
    }

    #[test]
    fn review_field_accepts_object() {
        let args: Args = serde_json::from_value(serde_json::json!({
            "prompt": "x",
            "review": {"repo": "o/r", "gate": "commit", "git_ref": "abc"}
        }))
        .unwrap();
        let review = args.review.unwrap();
        assert_eq!(review.repo, "o/r");
        assert_eq!(review.gate, "commit");
        assert_eq!(review.git_ref, "abc");
    }

    #[test]
    fn review_field_accepts_stringified_json() {
        let args: Args = serde_json::from_value(serde_json::json!({
            "prompt": "x",
            "review": "{\"repo\":\"o/r\",\"gate\":\"commit\",\"git_ref\":\"abc\"}"
        }))
        .unwrap();
        let review = args.review.unwrap();
        assert_eq!(review.repo, "o/r");
        assert_eq!(review.gate, "commit");
        assert_eq!(review.git_ref, "abc");
    }

    #[test]
    fn review_field_accepts_null_string() {
        let args: Args =
            serde_json::from_value(serde_json::json!({"prompt": "x", "review": "null"})).unwrap();
        assert!(args.review.is_none());
    }

    #[test]
    fn review_field_accepts_null() {
        let args: Args =
            serde_json::from_value(serde_json::json!({"prompt": "x", "review": null})).unwrap();
        assert!(args.review.is_none());
    }

    #[test]
    fn review_field_absent_defaults_none() {
        let args: Args = serde_json::from_value(serde_json::json!({"prompt": "x"})).unwrap();
        assert!(args.review.is_none());
    }

    #[test]
    fn allowlists_reference_real_tools() {
        let (catalog, _) = catalog_and_types();
        for name in EXPLORE_TOOLS
            .iter()
            .chain(WORKER_TOOLS)
            .chain(REVIEWER_TOOLS)
        {
            assert!(
                catalog.contains(name),
                "allowlist names unknown tool {name}"
            );
        }
    }

    /// The description advertises the toolsets to the root; keep it in
    /// sync with the allowlists.
    #[test]
    fn description_names_every_allowlisted_tool() {
        let tool = task_tool(Vec::new(), Tools::default(), Tools::default(), 1);
        let desc = tool.description();
        for name in EXPLORE_TOOLS.iter().chain(WORKER_TOOLS) {
            assert!(desc.contains(name), "description omits {name}");
        }
        assert!(desc.contains("reviewer"));
    }

    #[test]
    fn worker_allowlist_is_superset_of_explore() {
        for name in EXPLORE_TOOLS {
            assert!(WORKER_TOOLS.contains(name));
        }
    }

    #[test]
    fn reviewer_allowlist_matches_explore() {
        assert_eq!(REVIEWER_TOOLS, EXPLORE_TOOLS);
    }

    #[test]
    fn children_never_see_task() {
        let task: Arc<dyn Tool> =
            Arc::new(task_tool(vec![], Tools::default(), Tools::default(), 5));
        let base = Tools::new(vec![task], &[]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let types = build_agent_types(&base, Vec::new(), &McpTools::default(), dir.path(), 30);
        for tools in [
            &types.explore.tools,
            &types.worker.tools,
            &types.reviewer.tools,
        ] {
            assert!(!tool_names(tools).contains(&"task".to_string()));
        }
    }

    #[test]
    fn worker_tools_superset_of_explore_tools() {
        let (_, types) = catalog_and_types();
        let worker_names = tool_names(&types.worker.tools);
        for name in tool_names(&types.explore.tools) {
            assert!(worker_names.contains(&name), "worker missing {name}");
        }
    }

    #[test]
    fn engine_tools_reach_explore_and_worker_but_not_reviewer() {
        let engine_tool: Arc<dyn Tool> = Arc::new(MockTool::new("engine"));
        let dir = tempfile::tempdir().unwrap();
        let types = build_agent_types(
            &Tools::default(),
            vec![engine_tool],
            &McpTools::default(),
            dir.path(),
            30,
        );
        assert!(tool_names(&types.explore.tools).contains(&"mock".to_string()));
        assert!(tool_names(&types.worker.tools).contains(&"mock".to_string()));
        // The reviewer's independence from the parent's narrative is
        // the design point: no LCM retrieval tools (spec 23).
        assert!(!tool_names(&types.reviewer.tools).contains(&"mock".to_string()));
    }

    #[test]
    fn prompts_include_environment_block() {
        let engine_tool: Arc<dyn Tool> = Arc::new(MockTool::new("engine"));
        let dir = tempfile::tempdir().unwrap();
        let types = build_agent_types(
            &Tools::default(),
            vec![engine_tool],
            &McpTools::default(),
            dir.path(),
            30,
        );
        assert!(types.explore.system_prompt.contains("research agent"));
        assert!(types.worker.system_prompt.contains("task agent"));
        assert!(types.reviewer.system_prompt.contains("code reviewer"));
        for prompt in [&types.explore.system_prompt, &types.worker.system_prompt] {
            assert!(prompt.contains(&dir.path().display().to_string()));
            assert!(prompt.contains("Available tools: mock"));
        }
        // Every type gets the checkout-layout line and the exact
        // iteration budget: sub-agents receive repo paths and must not
        // guess how much room they have.
        for prompt in [
            &types.explore.system_prompt,
            &types.worker.system_prompt,
            &types.reviewer.system_prompt,
        ] {
            assert!(prompt.contains("projects/<owner>/<repo>"));
            assert!(prompt.contains("Iteration budget: 30 tool rounds"));
        }
        assert!(
            types
                .reviewer
                .system_prompt
                .contains(&dir.path().display().to_string())
        );
    }

    /// The worker has exec and writes files, so "done" has to mean the
    /// check ran. The parent names the command; this is the half that
    /// obliges the worker to use it.
    #[test]
    fn worker_prompt_requires_running_the_check() {
        assert!(WORKER_PROMPT.contains("Run the check before"));
        assert!(WORKER_PROMPT.contains("say so plainly"));
    }

    #[test]
    fn reviewer_prompt_mandates_findings_block() {
        assert!(REVIEWER_PROMPT.contains("```findings"));
        assert!(REVIEWER_PROMPT.contains("verdict"));
        assert!(REVIEWER_PROMPT.contains("must-fix"));
    }

    #[test]
    fn findings_retry_prompt_reemits_without_new_analysis() {
        assert!(FINDINGS_RETRY_PROMPT.contains("```findings"));
        assert!(FINDINGS_RETRY_PROMPT.contains("unchanged in substance"));
    }

    /// A convention file inside the artifact is the author's claim, so
    /// the reviewer takes conventions from the parent and never reads
    /// them out of the checkout it is judging.
    #[test]
    fn reviewer_prompt_refuses_conventions_from_the_artifact() {
        assert!(REVIEWER_PROMPT.contains("Do not go looking for `AGENTS.md`"));
        assert!(REVIEWER_PROMPT.contains("part of that artifact"));
        assert!(REVIEWER_PROMPT.contains("not a rule binding you"));
    }
}
