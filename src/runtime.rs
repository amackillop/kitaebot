//! Application runtime — assembles provider, tools, and channels.
//!
//! All `mock-network` conditional compilation for construction lives here,
//! keeping the rest of the codebase cfg-free.

use std::path::Path;

#[cfg(not(feature = "mock-network"))]
use tracing::error;

use crate::chat_completion::ChatCompletionsClient;
use crate::config::Config;
use crate::provider::CompletionsProvider;
use crate::telegram::TelegramChannel;
use crate::tools::path::PathGuard;
use crate::tools::{Exec, FileEdit, FileRead, FileWrite, GlobSearch, Grep, Tools};
use crate::workspace::Workspace;

/// Fully-assembled application runtime returned by [`build`].
pub struct Runtime {
    pub provider: CompletionsProvider<ChatCompletionsClient>,
    pub tools: Tools,
    pub telegram: Option<TelegramChannel>,
}

// ---------------------------------------------------------------------------
// Real build
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-network"))]
pub fn build(config: &Config, workspace: &Workspace, git_config: Option<&Path>) -> Runtime {
    use crate::secrets::load_secret;
    use crate::tools::{GitHub, WebFetch, WebSearch};

    let api_key = load_secret("provider-api-key").unwrap_or_else(|e| {
        error!("Failed to load API key: {e}");
        std::process::exit(1);
    });
    let telegram_token = if config.telegram.enabled {
        Some(load_secret("telegram-bot-token").unwrap_or_else(|e| {
            error!("Failed to load Telegram credentials: {e}");
            std::process::exit(1);
        }))
    } else {
        None
    };
    let github_token = if config.github.enabled {
        Some(load_secret("github-token").unwrap_or_else(|e| {
            error!("Failed to load GitHub token: {e}");
            std::process::exit(1);
        }))
    } else {
        None
    };

    let client = ChatCompletionsClient::new(api_key, config.provider.api.endpoint());
    let provider = CompletionsProvider::new(client.clone(), &config.provider);

    let mut tools = local_tools(workspace, config, git_config);

    if let Some(token) = github_token {
        tools.push(Box::new(GitHub::new(
            workspace.path(),
            token,
            git_config.map(Path::to_path_buf),
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

    let telegram = telegram_token.map(|t| TelegramChannel::new(t, &config.telegram));

    Runtime {
        provider,
        tools: Tools::new(tools),
        telegram,
    }
}

// ---------------------------------------------------------------------------
// Stub build (mock-network)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
pub fn build(config: &Config, workspace: &Workspace, git_config: Option<&Path>) -> Runtime {
    let client = ChatCompletionsClient;
    let provider = CompletionsProvider::new(client, &config.provider);
    let tools = local_tools(workspace, config, git_config);

    Runtime {
        provider,
        tools: Tools::new(tools),
        telegram: None,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Tools that work without network access.
fn local_tools(
    workspace: &Workspace,
    config: &Config,
    git_config: Option<&Path>,
) -> Vec<Box<dyn crate::tools::Tool>> {
    let guard = PathGuard::new(workspace.path());

    let exec = Exec::new(workspace.path(), &config.tools.exec);
    let exec = match git_config {
        Some(path) => exec.with_git_config(path.to_path_buf()),
        None => exec,
    };

    vec![
        Box::new(exec),
        Box::new(FileRead::new(guard.clone())),
        Box::new(FileWrite::new(guard.clone())),
        Box::new(FileEdit::new(guard.clone())),
        Box::new(GlobSearch::new(workspace.path())),
        Box::new(Grep::new(guard)),
    ]
}
