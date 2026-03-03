mod agent;
mod config;
mod daemon;
mod error;
mod heartbeat;
mod lock;
mod provider;
mod repl;
mod safety;
mod secrets;
mod session;
mod telegram;
mod tools;
mod types;
mod workspace;

use config::Config;
use heartbeat::Outcome;
use provider::Provider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use tools::{Exec, Tool, Tools};
use tracing::{error, info};
use workspace::Workspace;
#[cfg(not(feature = "mock-network"))]
use {provider::OpenRouterProvider, secrets::load_secret};

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

    #[cfg(feature = "mock-network")]
    let provider = StubProvider;

    #[cfg(not(feature = "mock-network"))]
    let provider = {
        let api_key = load_secret("openrouter-api-key").unwrap_or_else(|e| {
            error!("Failed to load API key: {e}");
            std::process::exit(1);
        });
        OpenRouterProvider::new(api_key, &config.provider)
    };

    let tools = Tools::new(vec![Tool::Exec(Exec::new(
        workspace.path(),
        &config.tools.exec,
    ))]);

    match std::env::args().nth(1).as_deref() {
        Some("chat") => {
            repl::run(&workspace, &provider, &tools, config.agent.max_iterations).await;
        }
        Some("heartbeat") => {
            run_heartbeat(&workspace, &provider, &tools, config.agent.max_iterations).await;
        }
        Some("run") => {
            let telegram = if config.telegram.enabled {
                Some(
                    telegram::TelegramChannel::new(&config.telegram).unwrap_or_else(|e| {
                        error!("Failed to load Telegram credentials: {e}");
                        std::process::exit(1);
                    }),
                )
            } else {
                None
            };

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

async fn run_heartbeat<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
) {
    match heartbeat::run(workspace, provider, tools, max_iterations).await {
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
