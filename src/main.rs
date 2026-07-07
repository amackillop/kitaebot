mod activity;
mod agent;
mod clients;
mod commands;
mod config;
mod daemon;
mod dispatch;
mod engine;
mod error;
mod github_channel;
mod heartbeat;
mod linear_channel;
mod notify;
mod provider;
mod runtime;
mod safety;
mod sandbox;
mod secrets;
mod session;
mod socket;
mod stats;
mod telegram;
mod time;
mod tools;
mod types;
mod workspace;

use std::sync::Arc;

use config::{Config, EngineKind};
use engine::{ContextEngine, ToolScope};
use tracing::{error, info, warn};
use workspace::Workspace;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kitaebot=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

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
    // files are inaccessible — secrets exist only in memory.
    let rt = runtime::build(&config, &workspace);

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
            let memory_dir = workspace.path().join("memory");
            let summarizer = config.provider.models.summarizer.as_deref();
            let summarize = engine::make_summarize_fn(role_provider(&provider, summarizer));

            let handle = match config.context.engine {
                EngineKind::Flat => {
                    let sessions_dir = workspace.path().join("sessions");
                    let engine =
                        engine::flat::FlatSession::new(sessions_dir, memory_dir, config.context)
                            .unwrap_or_else(|e| {
                                error!("Failed to initialize flat session: {e}");
                                std::process::exit(1);
                            });
                    spawn_with_engine(
                        workspace.clone(),
                        provider,
                        tools,
                        &config,
                        engine,
                        summarize,
                        rt.notifier.clone(),
                    )
                }
                EngineKind::Lcm => {
                    let db_path = memory_dir.join("lcm.db");
                    let engine = engine::lcm::LcmEngine::new(
                        &db_path,
                        memory_dir,
                        config.context,
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
fn spawn_with_engine<E: ContextEngine + 'static>(
    workspace: Arc<Workspace>,
    provider: Arc<provider::CompletionsProvider>,
    mut tools: tools::Tools,
    config: &Config,
    engine: E,
    summarize: engine::SummarizeFn,
    notifier: Option<Arc<notify::Notifier>>,
) -> agent::AgentHandle {
    let (explore, worker) =
        agent::task::build_agent_types(&tools, engine.tools(ToolScope::SubAgent), workspace.path());
    let models = &config.provider.models;
    let task_tool: Arc<dyn tools::Tool> = Arc::new(agent::task::TaskTool::new(
        role_provider(&provider, models.explore.as_deref()),
        role_provider(&provider, models.worker.as_deref()),
        summarize.clone(),
        explore,
        worker,
        config.sub_agents.max_iterations,
    ));
    tools.extend_with(engine.tools(ToolScope::Root), &config.tools.disabled);
    tools.extend_with(vec![task_tool], &config.tools.disabled);
    agent::AgentHandle::spawn(
        workspace,
        provider,
        Arc::new(tools),
        config.agent.max_iterations,
        engine,
        summarize,
        notifier,
    )
}
