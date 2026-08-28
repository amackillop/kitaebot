mod activity;
mod agent;
mod backup;
mod channel;
mod clients;
mod commands;
mod config;
mod confine;
mod context;
mod conventions;
mod daemon;
mod dispatch;
mod duty;
mod errlog;
mod error;
mod memory;
mod notify;
mod provider;
mod retry;
mod review;
mod runtime;
mod safety;
mod sandbox;
mod secrets;
mod sqlite;
mod state_db;
#[cfg(test)]
mod test_support;
mod time;
mod tools;
mod types;
mod usage;
mod workspace;

use std::path::Path;
use std::sync::Arc;

use config::{Config, EngineKind};
use context::{ContextEngine, ToolScope};
use tracing::{error, info, warn};
use workspace::Workspace;

/// Stderr logging plus the error tee (spec 24): WARN/ERROR events
/// mirrored to `state/errors/` as the self-analysis duty's symptom
/// feed. A tee that cannot be set up disables itself with a note —
/// logging must never stop the daemon. Returns the tee's flush guard;
/// hold it for the process lifetime.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(|| errlog::BoundedLineWriter::new(std::io::stderr(), errlog::LineFormat::Text))
        .with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kitaebot=info".into()),
        );
    let tee = workspace::resolve_root()
        .map(|root| {
            root.join(workspace::STATE_DIR)
                .join(workspace::ERRORS_SUBDIR)
        })
        .and_then(|dir| errlog::layer(&dir))
        .map_err(|e| eprintln!("error tee disabled: {e}"))
        .ok();
    let (tee_layer, guard) = match tee {
        Some((layer, guard)) => (Some(layer), Some(guard)),
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(stderr)
        .with(tee_layer)
        .init();
    errlog::install_panic_hook();
    guard
}

fn main() {
    // Hidden subcommand, dispatched before tracing: confine execs the
    // real command, and any log line here would land in its stderr.
    if std::env::args().nth(1).as_deref() == Some("confine") {
        confine::run();
    }
    daemon_main();
}

#[tokio::main]
async fn daemon_main() {
    let _errlog_guard = init_tracing();

    let workspace = Workspace::init().unwrap_or_else(|e| {
        error!("Failed to initialize workspace: {e}");
        std::process::exit(1);
    });

    let config = Config::load(workspace.path()).unwrap_or_else(|e| {
        error!("Failed to load config: {e}");
        std::process::exit(1);
    });

    let socket_path = std::path::Path::new(&config.socket.path);

    // Backup needs no secrets, no network, and no sandbox — stage and
    // exit before any of that is set up.
    if std::env::args().nth(1).as_deref() == Some("backup") {
        run_backup(&workspace, &config);
    }

    // Load all secrets before sandboxing. After enforcement, credential
    // files are inaccessible — secrets exist only in memory. MCP
    // children also spawn here: their credentials and binaries must
    // still be readable (spec 22).
    let rt = runtime::build(&config, &workspace);
    // The only call: MCP registrations are leaked to `'static`, so a
    // second one leaks a full set. See `mcp::start`.
    let mcp = tools::mcp::start(&config.mcp).await;

    let gnupg_home = std::env::var_os("GNUPGHOME").map(std::path::PathBuf::from);
    if let Err(e) = sandbox::apply(workspace.path(), socket_path, gnupg_home.as_deref()) {
        warn!("Sandbox not applied: {e}");
    }

    // --- Everything below runs under Landlock confinement ---

    match std::env::args().nth(1).as_deref() {
        Some("run") => {
            info!(telegram = config.telegram.enabled, "Daemon starting");

            let state_db = open_state_db(&workspace);
            let duties = build_duties(&config);
            let (duty_trigger, trigger_rx) = duty_trigger_channel(&duties);
            let workspace = Arc::new(workspace);
            let provider = Arc::new(rt.provider);
            let tools = rt.tools;
            let summarizer = config.provider.model_overrides.summarizer.as_ref();
            let summarize = context::make_summarize_fn(role_provider(&provider, summarizer));

            let handle = build_handle(
                workspace.clone(),
                &state_db,
                provider,
                tools,
                mcp,
                &config,
                summarize,
                rt.notifier.clone(),
                duty_trigger,
            );

            Box::pin(daemon::run(
                &workspace,
                &state_db,
                &handle,
                duties,
                rt.telegram.as_ref(),
                rt.github.as_ref(),
                rt.git_cli.as_ref(),
                &config.github,
                &config.git.trusted_repos(),
                rt.linear.as_ref(),
                &config.socket,
                trigger_rx,
            ))
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
            eprintln!("  run             Start daemon (duties + channels)");
            eprintln!("  backup <dir>    Stage durable state into <dir> (spec 05)");
            std::process::exit(1);
        }
    }
}

/// `kitaebot backup <dir>`: stage durable state and exit.
fn run_backup(workspace: &Workspace, config: &Config) -> ! {
    let Some(dest) = std::env::args().nth(2) else {
        eprintln!("usage: kitaebot backup <dir>");
        std::process::exit(2);
    };
    match backup::stage(
        workspace,
        config.effective_context().engine,
        Path::new(&dest),
    ) {
        Ok(_unclassified) => std::process::exit(0),
        Err(e) => {
            error!("backup staging failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Open the operational state database, exiting on failure. Fatal
/// because duty cadence and poll cursors live here, and a scheduler
/// running without them re-fires everything on restart — worse than
/// not starting.
fn open_state_db(workspace: &Workspace) -> state_db::StateDb {
    state_db::StateDb::open(&workspace.state_db_path()).unwrap_or_else(|e| {
        error!("Failed to open state database: {e}");
        std::process::exit(1);
    })
}

/// Construct the agent handle for the configured context engine.
/// Thresholds apply to the window minus the output reserve; see
/// `Config::effective_context`.
#[allow(clippy::too_many_arguments)]
fn build_handle(
    workspace: Arc<Workspace>,
    state_db: &state_db::StateDb,
    provider: Arc<provider::CompletionsProvider>,
    tools: tools::Tools,
    mcp: tools::mcp::McpTools,
    config: &Config,
    summarize: context::SummarizeFn,
    notifier: Option<Arc<notify::Notifier>>,
    duty_trigger: duty::TriggerHandle,
) -> agent::AgentHandle {
    let context = config.effective_context();
    match context.engine {
        EngineKind::Flat => {
            let engine = context::flat::FlatSession::new(&workspace.context_dir(), context)
                .unwrap_or_else(|e| {
                    error!("Failed to initialize flat session: {e}");
                    std::process::exit(1);
                });
            spawn_with_engine(
                workspace,
                state_db,
                provider,
                tools,
                mcp,
                config,
                engine,
                summarize,
                notifier,
                Some(duty_trigger),
            )
        }
        EngineKind::Lcm => {
            let engine =
                context::lcm::LcmEngine::new(&workspace.context_dir(), context, summarize.clone())
                    .unwrap_or_else(|e| {
                        error!("Failed to initialize LCM engine: {e}");
                        std::process::exit(1);
                    });
            spawn_with_engine(
                workspace,
                state_db,
                provider,
                tools,
                mcp,
                config,
                engine,
                summarize,
                notifier,
                Some(duty_trigger),
            )
        }
    }
}

/// Operator run-now channel: the actor validates and queues, the
/// scheduler executes on its shared path (spec 24).
fn duty_trigger_channel(
    duties: &[duty::Duty],
) -> (
    duty::TriggerHandle,
    tokio::sync::mpsc::Receiver<duty::Trigger>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let handle = duty::TriggerHandle {
        names: duties.iter().map(|d| d.name.clone()).collect(),
        tx,
    };
    (handle, rx)
}

/// Built-in and operator-defined duties, scheduled from config
/// (spec 24 phase 1).
fn build_duties(config: &Config) -> Vec<duty::Duty> {
    // Validated by Config::validate; parse cannot fail here.
    let parse =
        |s: &crate::config::ScheduleConfig| s.parse().expect("validated schedule failed to parse");
    let mut duties = vec![duty::Duty {
        name: "distill".into(),
        action: duty::Action::Dispatch {
            input: "/duty distill".into(),
            session_hint: None,
        },
        schedule: parse(&config.duties.distill),
        gate: None,
    }];
    duties.extend(config.duties.prompt.iter().map(|p| duty::Duty {
        name: p.name.clone(),
        action: duty::Action::Dispatch {
            input: p.prompt.clone(),
            session_hint: Some(p.repo.clone()),
        },
        schedule: parse(&p.schedule),
        gate: p.gate.as_deref().map(|_| duty::Gate::NewCommits {
            repo: p.repo.clone(),
        }),
    }));
    if let Some(sa) = &config.duties.self_analysis {
        duties.push(duty::Duty {
            name: "self-analysis".into(),
            action: duty::Action::SelfAnalysis {
                repo: sa.repo.clone(),
                min_delta_tokens: sa.min_delta_tokens,
            },
            schedule: parse(&sa.schedule),
            gate: None,
        });
    }
    // Last: a due-together cold warm must not delay the duties above.
    if !config.git.warm_commands().is_empty() {
        duties.push(duty::Duty {
            name: "warm".into(),
            action: duty::Action::Warm,
            schedule: parse(&config.duties.warm),
            gate: None,
        });
    }
    duties
}

/// Provider variant for a role: the override spec when configured,
/// otherwise the shared root provider.
fn role_provider(
    provider: &Arc<provider::CompletionsProvider>,
    spec: Option<&config::ModelSpec>,
) -> Arc<provider::CompletionsProvider> {
    match spec {
        Some(s) => Arc::new(provider.with_spec(s)),
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
    state_db: &state_db::StateDb,
    provider: Arc<provider::CompletionsProvider>,
    mut tools: tools::Tools,
    mcp: tools::mcp::McpTools,
    config: &Config,
    engine: E,
    summarize: context::SummarizeFn,
    notifier: Option<Arc<notify::Notifier>>,
    duty_trigger: Option<duty::TriggerHandle>,
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
        state_db.clone(),
        config.memory.distill_threshold_tokens,
        config.memory.effective_slice_tokens(),
        config.sub_agents.max_iterations,
        config.memory.index_cap_bytes,
    ));
    // Telemetry: an open failure is logged and the daemon runs
    // unmetered and unrecorded. Opened before the task tool so
    // sub-agent turns share the ledgers.
    let usage_ledger = usage::UsageLedger::new(state_db, config.usage.rates.clone());
    let usage_ledger = match config.provider.api.pricing_endpoint() {
        Some(base) => usage_ledger.with_pricing(clients::openrouter_pricing::PricingClient::new(
            base.to_string(),
        )),
        None => usage_ledger,
    };
    let usage_ledger = Some(Arc::new(usage_ledger));
    let review_ledger = config
        .review
        .enabled
        .then(|| Arc::new(review::ReviewLedger::new(state_db)));
    let overrides = &config.provider.model_overrides;
    let task_tool: Arc<dyn tools::Tool> = Arc::new(agent::task::TaskTool::new(
        agent::task::TypeProviders {
            explore: role_provider(&provider, overrides.explore.as_ref()),
            worker: role_provider(&provider, overrides.worker.as_ref()),
            reviewer: role_provider(&provider, overrides.reviewer.as_ref()),
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
        let review_disposition: Arc<dyn tools::Tool> =
            Arc::new(review::ReviewDispositionTool::new(ledger.clone()));
        tools.extend_with(vec![review_log, review_disposition], &config.tools.disabled);
    }
    let memory_provider = role_provider(&provider, overrides.memory.as_ref());
    agent::AgentHandle::spawn(
        workspace,
        provider,
        memory_provider,
        Arc::new(tools),
        distiller,
        config.agent.max_iterations,
        agent::PromptConfig {
            memory_index_cap: config.memory.index_cap_bytes,
            trusted_repos: config.git.trusted_repos(),
        },
        engine,
        summarize,
        notifier,
        usage_ledger,
        review_ledger,
        duty_trigger,
    )
}
