mod activity;
mod agent;
mod clients;
mod commands;
mod config;
mod context;
mod daemon;
mod dispatch;
mod error;
mod heartbeat;
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

use std::sync::Arc;

use config::Config;
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
            let handle = agent::AgentHandle::spawn(
                workspace.clone(),
                Arc::new(rt.provider),
                Arc::new(rt.tools),
                config.agent.max_iterations,
                config.context,
            );

            daemon::run(
                &workspace,
                &handle,
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
            eprintln!("  run  Start daemon (heartbeat + channels)");
            std::process::exit(1);
        }
    }
}
