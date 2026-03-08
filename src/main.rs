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
use chat_completion::ChatCompletionsClient;
use config::Config;
use heartbeat::Outcome;
use provider::{CompletionsProvider, Provider};
use tools::path::PathGuard;
use tools::{Exec, FileEdit, FileRead, FileWrite, GlobSearch, Grep, Tools};
use tracing::{error, info, warn};
use workspace::Workspace;
#[cfg(not(feature = "mock-network"))]
use {secrets::load_secret, tools::GitHub, tools::WebFetch, tools::WebSearch};

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

    // .gitconfig is provisioned externally (e.g. by the NixOS module)
    // alongside config.toml. If present, the exec tool sets
    // GIT_CONFIG_GLOBAL so child processes pick up git identity.
    let git_config_path = {
        let path = workspace.path().join(".gitconfig");
        path.exists().then_some(path)
    };

    // Load all secrets before sandboxing. After enforcement, credential
    // files are inaccessible — secrets exist only in memory.
    #[cfg(not(feature = "mock-network"))]
    let api_key = load_secret("provider-api-key").unwrap_or_else(|e| {
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
    #[cfg(not(feature = "mock-network"))]
    let github_token = if config.github.enabled {
        Some(load_secret("github-token").unwrap_or_else(|e| {
            error!("Failed to load GitHub token: {e}");
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

    #[cfg(not(feature = "mock-network"))]
    let client = ChatCompletionsClient::new(api_key, config.provider.api.endpoint());

    #[cfg(feature = "mock-network")]
    let client = ChatCompletionsClient;
    let provider = CompletionsProvider::new(client.clone(), &config.provider);

    let tools = build_tools(
        &workspace,
        &config,
        git_config_path.as_deref(),
        #[cfg(not(feature = "mock-network"))]
        client,
        #[cfg(not(feature = "mock-network"))]
        github_token,
    );

    let turn_config = TurnConfig {
        provider: &provider,
        tools: &tools,
        max_iterations: config.agent.max_iterations,
        context: &config.context,
    };

    match std::env::args().nth(1).as_deref() {
        Some("heartbeat") => {
            run_heartbeat(&workspace, &turn_config).await;
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
                &turn_config,
                config.heartbeat.interval_secs,
                telegram.as_ref(),
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

fn build_tools(
    workspace: &Workspace,
    config: &Config,
    git_config: Option<&std::path::Path>,
    #[cfg(not(feature = "mock-network"))] client: ChatCompletionsClient,
    #[cfg(not(feature = "mock-network"))] github_token: Option<secrets::Secret>,
) -> Tools {
    let guard = PathGuard::new(workspace.path());

    let exec = Exec::new(workspace.path(), &config.tools.exec);
    let exec = match git_config {
        Some(path) => exec.with_git_config(path.to_path_buf()),
        None => exec,
    };

    #[allow(unused_mut)]
    let mut tools: Vec<Box<dyn tools::Tool>> = vec![
        Box::new(exec),
        Box::new(FileRead::new(guard.clone())),
        Box::new(FileWrite::new(guard.clone())),
        Box::new(FileEdit::new(guard.clone())),
        Box::new(GlobSearch::new(workspace.path())),
        Box::new(Grep::new(guard.clone())),
    ];

    #[cfg(not(feature = "mock-network"))]
    {
        if let Some(token) = github_token {
            tools.push(Box::new(GitHub::new(
                workspace.path(),
                token,
                git_config.map(std::path::Path::to_path_buf),
                config.git.co_authors.clone(),
            )));
        }

        tools.push(Box::new(
            WebFetch::new(&config.tools.web_fetch).unwrap_or_else(|e| {
                error!("Failed to initialize web_fetch: {e}");
                std::process::exit(1);
            }),
        ));

        tools.push(Box::new(WebSearch::new(client, &config.tools.web_search)));
    }

    Tools::new(tools)
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
