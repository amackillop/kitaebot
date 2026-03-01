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
mod tools;
mod types;
mod workspace;

use config::Config;
use heartbeat::Outcome;
use provider::Provider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use tools::{Exec, Tool, Tools};
use workspace::Workspace;
#[cfg(not(feature = "mock-network"))]
use {provider::OpenRouterProvider, secrets::load_secret};

#[tokio::main]
async fn main() {
    let workspace = Workspace::init().unwrap_or_else(|e| {
        eprintln!("Failed to initialize workspace: {e}");
        std::process::exit(1);
    });

    let config = Config::load(workspace.path()).unwrap_or_else(|e| {
        eprintln!("Failed to load config: {e}");
        std::process::exit(1);
    });

    #[cfg(feature = "mock-network")]
    let provider = StubProvider;

    #[cfg(not(feature = "mock-network"))]
    let provider = {
        let api_key = load_secret("openrouter-api-key").unwrap_or_else(|e| {
            eprintln!("Failed to load API key: {e}");
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
            eprintln!(
                "kitaebot daemon starting (heartbeat every {}s)",
                config.heartbeat.interval_secs,
            );
            daemon::run(
                &workspace,
                &provider,
                &tools,
                config.agent.max_iterations,
                config.heartbeat.interval_secs,
            )
            .await;
        }
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
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
            eprintln!("Heartbeat complete: {response}");
        }
        Ok(Outcome::Skipped(reason)) => {
            eprintln!("Heartbeat skipped: {reason}");
        }
        Err(e) => {
            eprintln!("Heartbeat failed: {e}");
            std::process::exit(1);
        }
    }
}
