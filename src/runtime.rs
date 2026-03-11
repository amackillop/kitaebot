//! Application runtime — assembles provider, tools, and channels.
//!
//! All `mock-network` conditional compilation for construction lives here,
//! keeping the rest of the codebase cfg-free.

use tracing::error;

use crate::config::Config;
use crate::provider::CompletionsProvider;
use crate::telegram::Telegram;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// Fully-assembled application runtime returned by [`build`].
pub struct Runtime {
    pub provider: CompletionsProvider,
    pub tools: Tools,
    pub telegram: Option<Telegram>,
}

// ---------------------------------------------------------------------------
// Real build
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-network"))]
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    use std::time::Duration;

    use crate::clients::chat_completion::{CompletionsClient, RealCompletionsApi};
    use crate::clients::telegram::{RealTelegramApi, TelegramClient};
    use crate::secrets::load_secret;
    use crate::tools::network;

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

    let api = RealCompletionsApi::new(config.provider.api.endpoint()).unwrap_or_else(|e| {
        error!("Failed to build client: {e}");
        std::process::exit(1);
    });
    let client = CompletionsClient::new(api);
    let provider = CompletionsProvider::new(client.clone(), &config.provider);

    let mut tools = Tools::local(workspace, config);
    tools.extend(network::build(workspace, config, client, github_token));

    let telegram = telegram_token.map(|token| {
        let api = RealTelegramApi::new(
            token,
            Duration::from_secs(config.telegram.poll_timeout_secs + 10),
        );
        let tg_client = TelegramClient::new(api);
        Telegram::new(tg_client, config.telegram.chat_id)
    });

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
    use crate::clients::chat_completion::{CompletionsClient, MockNetworkApi};

    let client = CompletionsClient::new(MockNetworkApi);
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
