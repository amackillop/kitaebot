mod agent;
mod config;
mod error;
mod heartbeat;
mod lock;
mod provider;
mod repl;
mod session;
mod tools;
mod types;
mod workspace;

use config::Config;
use heartbeat::Outcome;
#[cfg(not(feature = "mock-network"))]
use provider::OpenRouterProvider;
use provider::Provider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use tools::{Exec, Tool, Tools};
use workspace::Workspace;

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
        let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| {
            eprintln!("Set OPENROUTER_API_KEY environment variable");
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
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: kitaebot <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  chat       Interactive conversation");
            eprintln!("  heartbeat  Run periodic tasks");
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
