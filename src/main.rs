mod activity;
mod agent;
mod chat_completion;
mod commands;
mod config;
mod context;
mod daemon;
mod dispatch;
mod error;
mod heartbeat;
mod lock;
mod provider;
mod runtime;
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

use agent::TurnConfig;
use config::Config;
use heartbeat::Outcome;
use provider::Provider;
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

    // .gitconfig is provisioned externally (e.g. by the NixOS module)
    // alongside config.toml. If present, the exec tool sets
    // GIT_CONFIG_GLOBAL so child processes pick up git identity.
    let git_config_path = {
        let path = workspace.path().join(".gitconfig");
        path.exists().then_some(path)
    };

    let socket_path = std::path::Path::new(&config.socket.path);

    // Load all secrets before sandboxing. After enforcement, credential
    // files are inaccessible — secrets exist only in memory.
    let rt = runtime::build(&config, &workspace, git_config_path.as_deref());

    if let Err(e) = sandbox::apply(workspace.path(), socket_path) {
        warn!("Sandbox not applied: {e}");
    }

    // --- Everything below runs under Landlock confinement ---

    let turn_config = TurnConfig {
        provider: &rt.provider,
        tools: &rt.tools,
        max_iterations: config.agent.max_iterations,
        context: &config.context,
    };

    match std::env::args().nth(1).as_deref() {
        Some("heartbeat") => {
            run_heartbeat(&workspace, &turn_config).await;
        }
        Some("run") => {
            info!(
                interval_secs = config.heartbeat.interval_secs,
                telegram = config.telegram.enabled,
                "Daemon starting",
            );
            daemon::run(
                &workspace,
                &turn_config,
                config.heartbeat.interval_secs,
                rt.telegram.as_ref(),
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
            eprintln!("  heartbeat  One-shot heartbeat cycle");
            eprintln!("  run        Start daemon (heartbeat + channels)");
            std::process::exit(1);
        }
    }
}

async fn run_heartbeat<P: Provider>(workspace: &Workspace, config: &TurnConfig<'_, P>) {
    match heartbeat::run(workspace, config).await {
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
