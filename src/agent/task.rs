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

use crate::engine::SummarizeFn;
use crate::engine::ephemeral::EphemeralSession;
use crate::error::ToolError;
use crate::provider::Provider;
use crate::tools::{Tool, ToolCtx, Tools};

use super::run_turn;

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

const EXPLORE_PROMPT: &str = "You are a research agent. Your job is to \
find information and report back.\n\nBe concise and specific. Include \
file paths, line numbers, and code snippets when relevant. Do not \
speculate — only report what you find.\n\nReturn your findings as a \
direct answer to the task. Your response will be read by another \
agent, not a human.";

const WORKER_PROMPT: &str = "You are a task agent. Complete the \
assigned task and report what you did.\n\nBe concise. Describe what \
you changed and why. Include file paths and relevant details. Your \
response will be read by another agent, not a human.";

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
        "{role}\n\n# Environment\nWorking directory: {}\nAvailable tools: {}",
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
    provider: Arc<P>,
    summarize: SummarizeFn,
    explore: AgentType,
    worker: AgentType,
    max_iterations: usize,
}

impl<P: Provider> TaskTool<P> {
    pub fn new(
        provider: Arc<P>,
        summarize: SummarizeFn,
        explore: AgentType,
        worker: AgentType,
        max_iterations: usize,
    ) -> Self {
        Self {
            provider,
            summarize,
            explore,
            worker,
            max_iterations,
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
        agent_type \"explore\" (default): read-only research. Cannot modify \
        files.\n\
        agent_type \"worker\": can read, write, and execute commands. For \
        self-contained tasks. Cannot use git or GitHub."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

            let agent = match args.agent_type {
                AgentKind::Explore => &self.explore,
                AgentKind::Worker => &self.worker,
            };

            // Fresh context per call, discarded on return. The token is
            // never cancelled: parent cancellation drops this future
            // instead (the parent's loop races tool execution against
            // its own token).
            let mut engine = EphemeralSession::new();
            run_turn(
                &mut engine,
                &self.summarize,
                &agent.system_prompt,
                &args.prompt,
                &*self.provider,
                &agent.tools,
                self.max_iterations,
                &ToolCtx::default(),
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("sub-agent failed: {e}")))
        })
    }
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
        TaskTool::new(
            Arc::new(MockProvider::new(responses)),
            noop_summarize(),
            agent_type(explore),
            agent_type(worker),
            max_iterations,
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
            provider,
            noop_summarize(),
            agent_type(Tools::default()),
            agent_type(mock_tools()),
            5,
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
