//! The `task` tool: delegated sub-agent turns.
//!
//! See `specs/19-sub-agents.md` for the design. A `task` call runs the
//! same `run_turn` loop the parent uses, against a fresh
//! [`EphemeralSession`], with a per-type tool allowlist. The child's
//! tool calls and reasoning stay in its own context; the parent sees
//! only the final assistant text.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::activity::Activity;
use crate::engine::SummarizeFn;
use crate::engine::ephemeral::EphemeralSession;
use crate::error::ToolError;
use crate::provider::Provider;
use crate::tools::{Tool, ToolCtx, Tools};
use crate::usage::{self, TurnRecord, UsageLedger};

use super::run_turn_metered;

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

/// Tool output cap (estimated tokens) for sub-agent contexts.
///
/// Deliberately far above the root's `context.tool_output_tokens`:
/// sub-agents exist to absorb verbose output, and `lcm_expand` may
/// legitimately return up to `MAX_EXPAND_TOKEN_CAP` (20k) in one tool
/// result. Matching it means expansion output survives untruncated.
const SUB_AGENT_TOOL_OUTPUT_TOKENS: usize = 20_000;

const EXPLORE_PROMPT: &str = include_str!("../prompts/explore.md");

const WORKER_PROMPT: &str = include_str!("../prompts/worker.md");

/// A sub-agent type: system prompt plus prebuilt tool set.
pub(crate) struct AgentType {
    system_prompt: String,
    tools: Tools,
}

/// Build the `explore` and `worker` agent types from the parent's
/// base registry (post-`tools.disabled`) and the engine's sub-agent
/// tools. Both children share the engine tool instances; neither can
/// see `task`, so recursion is structurally impossible.
pub(crate) fn build_agent_types(
    base: &Tools,
    engine_tools: Vec<Arc<dyn Tool>>,
    workspace_dir: &Path,
) -> (AgentType, AgentType) {
    let mut explore_tools = base.filtered(EXPLORE_TOOLS);
    explore_tools.extend_with(engine_tools.clone(), &[]);
    let mut worker_tools = base.filtered(WORKER_TOOLS);
    worker_tools.extend_with(engine_tools, &[]);

    let explore = AgentType {
        system_prompt: compose_prompt(EXPLORE_PROMPT, workspace_dir, &explore_tools),
        tools: explore_tools,
    };
    let worker = AgentType {
        system_prompt: compose_prompt(WORKER_PROMPT, workspace_dir, &worker_tools),
        tools: worker_tools,
    };
    (explore, worker)
}

/// Append environment info (working directory, tool names) to a
/// type's role prompt, mirroring the parent's system prompt assembly.
fn compose_prompt(role: &str, workspace_dir: &Path, tools: &Tools) -> String {
    let names: Vec<String> = tools
        .definitions()
        .into_iter()
        .map(|d| d.function.name)
        .collect();
    format!(
        "{}\n\n# Environment\nWorking directory: {}\nAvailable tools: {}",
        role.trim_end(),
        workspace_dir.display(),
        names.join(", "),
    )
}

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AgentKind {
    #[default]
    Explore,
    Worker,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// The task for the sub-agent to perform. The sub-agent cannot see
    /// the conversation history; include all necessary context.
    prompt: String,
    /// "explore" (default): read-only research. "worker": can also
    /// write files and execute commands.
    #[serde(default)]
    agent_type: AgentKind,
}

/// Tool that runs a sub-agent turn in an isolated in-memory context.
///
/// Generic over the provider because [`Provider`] is not object-safe;
/// the registry erases the generic behind `Arc<dyn Tool>`.
pub(crate) struct TaskTool<P: Provider> {
    explore_provider: Arc<P>,
    worker_provider: Arc<P>,
    summarize: SummarizeFn,
    explore: AgentType,
    worker: AgentType,
    max_iterations: usize,
    usage_ledger: Option<Arc<UsageLedger>>,
}

impl<P: Provider> TaskTool<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        explore_provider: Arc<P>,
        worker_provider: Arc<P>,
        summarize: SummarizeFn,
        explore: AgentType,
        worker: AgentType,
        max_iterations: usize,
        usage_ledger: Option<Arc<UsageLedger>>,
    ) -> Self {
        Self {
            explore_provider,
            worker_provider,
            summarize,
            explore,
            worker,
            max_iterations,
            usage_ledger,
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
        tools."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
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
                AgentKind::Explore => (&self.explore, &self.explore_provider, "explore"),
                AgentKind::Worker => (&self.worker, &self.worker_provider, "worker"),
            };

            // Fresh context per call, discarded on return. The parent's
            // token is threaded through so the child can observe
            // cancellation at an iteration boundary, though the primary
            // cancel path is still the parent dropping this future.
            let mut engine = EphemeralSession::new(SUB_AGENT_TOOL_OUTPUT_TOKENS);
            let child_ctx = ToolCtx {
                activity: ctx.activity.as_ref().map(|parent| forward(parent, label)),
                cancel: ctx.cancel.clone(),
            };
            let (output, usage) = run_turn_metered(
                &mut engine,
                &self.summarize,
                &agent.system_prompt,
                &args.prompt,
                &**provider,
                &agent.tools,
                self.max_iterations,
                &child_ctx,
            )
            .instrument(tracing::info_span!("subagent", agent = label))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("sub-agent failed: {e}")))?;
            // No conversation of its own: the row is tagged by agent type.
            usage::record_turn(
                self.usage_ledger.as_deref(),
                &TurnRecord {
                    session: "subagent",
                    source: label,
                    model: provider.model(),
                    usage,
                },
            );
            Ok(output.into_text())
        })
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
                    name: "mock".to_string(),
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

    fn task_tool(
        responses: Vec<Result<Response, ProviderError>>,
        explore: Tools,
        worker: Tools,
        max_iterations: usize,
    ) -> TaskTool<MockProvider> {
        let provider = Arc::new(MockProvider::new(responses));
        TaskTool::new(
            provider.clone(),
            provider,
            noop_summarize(),
            agent_type(explore),
            agent_type(worker),
            max_iterations,
            None,
        )
    }

    fn mock_tools() -> Tools {
        Tools::new(vec![Arc::new(MockTool::new("mock output"))], &[]).unwrap()
    }

    /// Full local tool catalog plus the cfg-gated network names.
    fn catalog_and_types() -> (Vec<&'static str>, AgentType, AgentType) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        let config = crate::config::Config::load(dir.path()).unwrap();
        let local = Tools::local(&workspace, &config, DirenvCache::new());
        let mut names: Vec<&'static str> = local.iter().map(|t| t.name()).collect();
        // Network tools are cfg-gated out under mock-network; their
        // names are pinned by spec 03.
        names.extend(["web_fetch", "web_search"]);
        let base = Tools::new(local, &[]).unwrap();
        let (explore, worker) = build_agent_types(&base, Vec::new(), workspace.path());
        (names, explore, worker)
    }

    fn tool_names(tools: &Tools) -> Vec<String> {
        tools
            .definitions()
            .into_iter()
            .map(|d| d.function.name)
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
            provider.clone(),
            provider,
            noop_summarize(),
            agent_type(Tools::default()),
            agent_type(mock_tools()),
            5,
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
        let tool = TaskTool::new(
            explore_provider.clone(),
            worker_provider.clone(),
            noop_summarize(),
            agent_type(Tools::default()),
            agent_type(Tools::default()),
            5,
            None,
        );

        let explored = tool
            .execute(
                serde_json::json!({"prompt": "x", "agent_type": "explore"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(explored, "from explore provider");

        let worked = tool
            .execute(
                serde_json::json!({"prompt": "x", "agent_type": "worker"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(worked, "from worker provider");

        assert_eq!(explore_provider.call_count(), 1);
        assert_eq!(worker_provider.call_count(), 1);
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
            Arc::clone(&provider),
            Arc::clone(&provider),
            noop_summarize(),
            agent_type(Tools::default()),
            agent_type(Tools::default()),
            5,
            None,
        );
        let ctx = ToolCtx::default();
        ctx.cancel.cancel();
        let err = tool
            .execute(serde_json::json!({"prompt": "x"}), ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::ExecutionFailed(msg) => assert!(msg.contains("sub-agent failed")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn max_iterations_surfaces_as_tool_error() {
        let max = 3;
        let tool = task_tool(
            vec![Ok(mock_tool_calls()); max],
            mock_tools(),
            Tools::default(),
            max,
        );
        let err = tool
            .execute(
                serde_json::json!({"prompt": "loop forever"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::ExecutionFailed(msg) => assert!(msg.contains("sub-agent failed")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
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
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
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
    fn allowlists_reference_real_tools() {
        let (catalog, _, _) = catalog_and_types();
        for name in EXPLORE_TOOLS.iter().chain(WORKER_TOOLS) {
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
    }

    #[test]
    fn worker_allowlist_is_superset_of_explore() {
        for name in EXPLORE_TOOLS {
            assert!(WORKER_TOOLS.contains(name));
        }
    }

    #[test]
    fn children_never_see_task() {
        let task: Arc<dyn Tool> =
            Arc::new(task_tool(vec![], Tools::default(), Tools::default(), 5));
        let base = Tools::new(vec![task], &[]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (explore, worker) = build_agent_types(&base, Vec::new(), dir.path());
        assert!(!tool_names(&explore.tools).contains(&"task".to_string()));
        assert!(!tool_names(&worker.tools).contains(&"task".to_string()));
    }

    #[test]
    fn worker_tools_superset_of_explore_tools() {
        let (_, explore, worker) = catalog_and_types();
        let worker_names = tool_names(&worker.tools);
        for name in tool_names(&explore.tools) {
            assert!(worker_names.contains(&name), "worker missing {name}");
        }
    }

    #[test]
    fn engine_tools_appended_to_both_types() {
        let engine_tool: Arc<dyn Tool> = Arc::new(MockTool::new("engine"));
        let dir = tempfile::tempdir().unwrap();
        let (explore, worker) = build_agent_types(&Tools::default(), vec![engine_tool], dir.path());
        assert!(tool_names(&explore.tools).contains(&"mock".to_string()));
        assert!(tool_names(&worker.tools).contains(&"mock".to_string()));
    }

    #[test]
    fn prompts_include_environment_block() {
        let engine_tool: Arc<dyn Tool> = Arc::new(MockTool::new("engine"));
        let dir = tempfile::tempdir().unwrap();
        let (explore, worker) = build_agent_types(&Tools::default(), vec![engine_tool], dir.path());
        assert!(explore.system_prompt.contains("research agent"));
        assert!(worker.system_prompt.contains("task agent"));
        for prompt in [&explore.system_prompt, &worker.system_prompt] {
            assert!(prompt.contains(&dir.path().display().to_string()));
            assert!(prompt.contains("Available tools: mock"));
        }
    }
}
