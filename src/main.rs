mod agent;
mod commands;
mod config;
mod context;
mod daemon;
mod error;
mod heartbeat;
mod lock;
mod provider;
mod repl;
mod safety;
mod sandbox;
mod secrets;
mod session;
mod socket;
mod stats;
mod telegram;
mod tools;
mod types;
mod workspace;

use config::Config;
use heartbeat::Outcome;
use provider::Provider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use tools::path::PathGuard;
use tools::{Exec, FileEdit, FileRead, FileWrite, GlobSearch, Grep, Tools};
use tracing::{error, info, warn};
use workspace::Workspace;
#[cfg(not(feature = "mock-network"))]
use {provider::OpenRouterProvider, secrets::load_secret, tools::WebFetch, tools::WebSearch};

#[tokio::main]
#[allow(clippy::too_many_lines)]
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

    // Load all secrets before sandboxing. After enforcement, credential
    // files are inaccessible — secrets exist only in memory.
    #[cfg(not(feature = "mock-network"))]
    let api_key = load_secret("openrouter-api-key").unwrap_or_else(|e| {
        error!("Failed to load API key: {e}");
        std::process::exit(1);
    });
    #[cfg(not(feature = "mock-network"))]
    let telegram_token = if config.telegram.enabled {
        Some(load_secret("telegram-bot-token").unwrap_or_else(|e| {
            error!("Failed to load Telegram credentials: {e}");
            std::process::exit(1);
        }))
    } else {
        None
    };

    let socket_path = std::path::Path::new(&config.socket.path);

    if let Err(e) = sandbox::apply(workspace.path(), socket_path) {
        warn!("Sandbox not applied: {e}");
    }

    // --- Everything below runs under Landlock confinement ---

    #[cfg(feature = "mock-network")]
    let provider = StubProvider;

    #[cfg(not(feature = "mock-network"))]
    let provider = OpenRouterProvider::new(api_key.clone(), &config.provider);

    let tools = build_tools(
        &workspace,
        &config,
        #[cfg(not(feature = "mock-network"))]
        api_key,
    );

    match std::env::args().nth(1).as_deref() {
        Some("chat") => {
            repl::run(
                &workspace,
                &provider,
                &tools,
                config.agent.max_iterations,
                &config.context,
            )
            .await;
        }
        Some("heartbeat") => {
            run_heartbeat(
                &workspace,
                &provider,
                &tools,
                config.agent.max_iterations,
                &config.context,
            )
            .await;
        }
        Some("run") => {
            #[cfg(not(feature = "mock-network"))]
            let telegram =
                telegram_token.map(|t| telegram::TelegramChannel::new(t, &config.telegram));

            #[cfg(feature = "mock-network")]
            let telegram: Option<telegram::TelegramChannel> = None;

            info!(
                interval_secs = config.heartbeat.interval_secs,
                telegram = config.telegram.enabled,
                "Daemon starting",
            );
            daemon::run(
                &workspace,
                &provider,
                &tools,
                config.agent.max_iterations,
                config.heartbeat.interval_secs,
                telegram.as_ref(),
                socket_path,
                &config.context,
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
            eprintln!("  chat       Interactive conversation");
            eprintln!("  heartbeat  One-shot heartbeat cycle");
            eprintln!("  run        Start daemon (heartbeat loop)");
            std::process::exit(1);
        }
    }
}

fn build_tools(
    workspace: &Workspace,
    config: &Config,
    #[cfg(not(feature = "mock-network"))] search_key: secrets::Secret,
) -> Tools {
    let guard = PathGuard::new(workspace.path());

    #[allow(unused_mut)]
    let mut tools: Vec<Box<dyn tools::Tool>> = vec![
        Box::new(Exec::new(workspace.path(), &config.tools.exec)),
        Box::new(FileRead::new(guard.clone())),
        Box::new(FileWrite::new(guard.clone())),
        Box::new(FileEdit::new(guard.clone())),
        Box::new(GlobSearch::new(workspace.path())),
        Box::new(Grep::new(guard.clone())),
    ];

    #[cfg(not(feature = "mock-network"))]
    {
        tools.push(Box::new(
            WebFetch::new(&config.tools.web_fetch).unwrap_or_else(|e| {
                error!("Failed to initialize web_fetch: {e}");
                std::process::exit(1);
            }),
        ));

        tools.push(Box::new(
            WebSearch::new(search_key, &config.tools.web_search).unwrap_or_else(|e| {
                error!("Failed to initialize web_search: {e}");
                std::process::exit(1);
            }),
        ));
    }

    Tools::new(tools)
}

async fn run_heartbeat<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    context: &config::ContextConfig,
) {
    match heartbeat::run(workspace, provider, tools, max_iterations, context).await {
        Ok(Outcome::Executed(response)) => {
            info!("Heartbeat complete: {response}");
        }
        Ok(Outcome::Skipped(reason)) => {
            info!("Heartbeat skipped: {reason}");
        }
        Err(e) => {
            error!("Heartbeat failed: {e}");
            std::process::exit(1);
        }
    }
}
