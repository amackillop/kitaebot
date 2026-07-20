mod activity;
mod agent;
mod channel;
mod clients;
mod commands;
mod config;
mod daemon;
mod dispatch;
mod engine;
mod error;
mod heartbeat;
mod memory;
mod notify;
mod provider;
mod retry;
mod review;
mod runtime;
mod safety;
mod sandbox;
mod secrets;
mod session;
#[cfg(test)]
mod test_support;
mod time;
mod tools;
mod types;
mod usage;
mod workspace;

use std::sync::Arc;

use config::{Config, EngineKind};
use engine::{ContextEngine, ToolScope};
use tracing::{error, info, warn};
use workspace::Workspace;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kitaebot=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let workspace = Workspace::init().unwrap_or_else(|e| {
        error!("Failed to initialize workspace: {e}");
        std::process::exit(1);
    });

    let config = Config::load(workspace.path()).unwrap_or_else(|e| {
        error!("Failed to load config: {e}");
        std::process::exit(1);
    });

    let socket_path = std::path::Path::new(&config.socket.path);

    // Load all secrets before sandboxing. After enforcement, credential
    // files are inaccessible — secrets exist only in memory. MCP
    // children also spawn here: their credentials and binaries must
    // still be readable (spec 22).
    let rt = runtime::build(&config, &workspace);
    let mcp = tools::mcp::start(&config.mcp).await;

    if let Err(e) = sandbox::apply(workspace.path(), socket_path) {
        warn!("Sandbox not applied: {e}");
    }

    // --- Everything below runs under Landlock confinement ---

    match std::env::args().nth(1).as_deref() {
        Some("run") => {
            info!(
                interval_secs = config.heartbeat.interval_secs,
                telegram = config.telegram.enabled,
                "Daemon starting",
            );

            let workspace = Arc::new(workspace);
            let provider = Arc::new(rt.provider);
            let tools = rt.tools;
            let state_dir = workspace.state_dir();
            let summarizer = config.provider.model_overrides.summarizer.as_deref();
            let summarize = engine::make_summarize_fn(role_provider(&provider, summarizer));

            // Thresholds apply to the window minus the output reserve;
            // see Config::effective_context.
            let context = config.effective_context();
            let handle = match context.engine {
                EngineKind::Flat => {
                    let sessions_dir = workspace.sessions_dir();
                    let engine = engine::flat::FlatSession::new(sessions_dir, state_dir, context)
                        .unwrap_or_else(|e| {
                            error!("Failed to initialize flat session: {e}");
                            std::process::exit(1);
                        });
                    spawn_with_engine(
                        workspace.clone(),
                        provider,
                        tools,
                        mcp,
                        &config,
                        engine,
                        summarize,
                        rt.notifier.clone(),
                    )
                }
                EngineKind::Lcm => {
                    let db_path = state_dir.join("lcm.db");
                    let engine = engine::lcm::LcmEngine::new(
                        &db_path,
                        state_dir,
                        context,
                        summarize.clone(),
                    )
                    .unwrap_or_else(|e| {
                        error!("Failed to initialize LCM engine: {e}");
                        std::process::exit(1);
                    });
                    spawn_with_engine(
                        workspace.clone(),
                        provider,
                        tools,
                        mcp,
                        &config,
                        engine,
                        summarize,
                        rt.notifier.clone(),
                    )
                }
            };

            daemon::run(
                &workspace,
                &handle,
                config.heartbeat.interval_secs,
                rt.telegram.as_ref(),
                rt.gh_cli.as_ref(),
                rt.git_cli.as_ref(),
                &config.github,
                rt.linear.as_ref(),
                socket_path,
            )
            .await;
        }
        Some(cmd) => {
            error!("Unknown command: {cmd}");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: kitaebot <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  run  Start daemon (heartbeat + channels)");
            std::process::exit(1);
        }
    }
}

/// Provider variant for a role: the override model when configured,
/// otherwise the shared root provider.
fn role_provider(
    provider: &Arc<provider::CompletionsProvider>,
    model: Option<&str>,
) -> Arc<provider::CompletionsProvider> {
    match model {
        Some(m) => Arc::new(provider.with_model(m)),
        None => provider.clone(),
    }
}

/// Build the `task` tool, merge the engine's root tools, and spawn
/// the agent actor.
///
/// Order matters: the child agent types are built from the base
/// registry *before* root engine tools are merged. Sub-agents get
/// their engine tools via `ToolScope::SubAgent` (includes
/// `lcm_expand`); the root gets `ToolScope::Root` (does not).
#[allow(clippy::too_many_arguments)]
fn spawn_with_engine<E: ContextEngine + 'static>(
    workspace: Arc<Workspace>,
    provider: Arc<provider::CompletionsProvider>,
    mut tools: tools::Tools,
    mcp: tools::mcp::McpTools,
    config: &Config,
    engine: E,
    summarize: engine::SummarizeFn,
    notifier: Option<Arc<notify::Notifier>>,
) -> agent::AgentHandle {
    // Namespacing makes collisions unlikely; if one happens anyway,
    // the built-in wins and the MCP tool is dropped everywhere.
    let mcp = mcp.without_collisions(&tools);
    let agent_types = agent::task::build_agent_types(
        &tools,
        engine.tools(ToolScope::SubAgent),
        &mcp,
        workspace.path(),
        config.sub_agents.max_iterations,
    );
    // The distiller mirrors a worker: built from the base registry
    // (memory-editing tools only) and capped at the sub-agent iteration
    // budget, before root engine tools and the task tool are merged in.
    let distiller = Arc::new(memory::distill::Distiller::new(
        &tools,
        workspace.path(),
        config.memory.distill_threshold_tokens,
        config.sub_agents.max_iterations,
    ));
    // Telemetry: an open failure is logged and the daemon runs unmetered.
    // Opened before the task tool so sub-agent turns share the ledger.
    let usage_ledger = match usage::UsageLedger::open(&workspace.usage_db_path()) {
        Ok(ledger) => Some(Arc::new(ledger)),
        Err(e) => {
            warn!("Usage ledger disabled: {e}");
            None
        }
    };
    // Telemetry, same contract as the usage ledger: open failure logs
    // and the pipeline runs unrecorded.
    let review_ledger = if config.review.enabled {
        match review::ReviewLedger::open(&workspace.review_db_path()) {
            Ok(ledger) => Some(Arc::new(ledger)),
            Err(e) => {
                warn!("Review ledger disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    let overrides = &config.provider.model_overrides;
    let task_tool: Arc<dyn tools::Tool> = Arc::new(agent::task::TaskTool::new(
        agent::task::TypeProviders {
            explore: role_provider(&provider, overrides.explore.as_deref()),
            worker: role_provider(&provider, overrides.worker.as_deref()),
            reviewer: role_provider(&provider, overrides.reviewer.as_deref()),
        },
        summarize.clone(),
        agent_types,
        config.sub_agents.max_iterations,
        usage_ledger.clone(),
        review_ledger.clone(),
    ));
    tools.extend_with(engine.tools(ToolScope::Root), &config.tools.disabled);
    tools.extend_with(vec![task_tool], &config.tools.disabled);
    tools.extend_with(mcp.all, &config.tools.disabled);
    // Root-only: children are built above from the base registry and
    // no allowlist names it.
    if let Some(ledger) = &review_ledger {
        let review_log: Arc<dyn tools::Tool> = Arc::new(review::ReviewLogTool::new(ledger.clone()));
        tools.extend_with(vec![review_log], &config.tools.disabled);
    }
    let heartbeat_provider = role_provider(&provider, overrides.heartbeat.as_deref());
    let memory_provider = role_provider(&provider, overrides.memory.as_deref());
    agent::AgentHandle::spawn(
        workspace,
        provider,
        heartbeat_provider,
        memory_provider,
        Arc::new(tools),
        distiller,
        config.agent.max_iterations,
        config.memory.index_cap_bytes,
        engine,
        summarize,
        notifier,
        usage_ledger,
        review_ledger,
    )
}
