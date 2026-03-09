//! Application runtime — assembles provider, tools, and channels.
//!
//! All `mock-network` conditional compilation for construction lives here,
//! keeping the rest of the codebase cfg-free.

use tracing::error;

use crate::chat_completion::ChatCompletionsClient;
use crate::config::Config;
use crate::provider::CompletionsProvider;
use crate::telegram::TelegramChannel;
use crate::tools::Tools;
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
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    use crate::secrets::load_secret;
    use crate::tools::network;

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

    let mut tools = Tools::local(workspace, config);
    tools.extend(network::build(workspace, config, client, github_token));

    let telegram = telegram_token.map(|t| TelegramChannel::new(t, &config.telegram));

    Runtime {
        provider,
        tools: Tools::new(tools, &config.tools.disabled).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(1);
        }),
        telegram,
    }
}

// ---------------------------------------------------------------------------
// Stub build (mock-network)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    let client = ChatCompletionsClient;
    let provider = CompletionsProvider::new(client, &config.provider);
    let tools = Tools::local(workspace, config);

    Runtime {
        provider,
        tools: Tools::new(tools, &config.tools.disabled).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(1);
        }),
        telegram: None,
    }
}
