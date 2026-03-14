//! Application runtime — assembles provider, tools, and channels.
//!
//! All `mock-network` conditional compilation for construction lives here,
//! keeping the rest of the codebase cfg-free.

use tracing::error;

use crate::config::Config;
use crate::provider::CompletionsProvider;
use crate::telegram::TelegramChannel;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// Fully-assembled application runtime returned by [`build`].
pub struct Runtime {
    pub provider: CompletionsProvider,
    pub tools: Tools,
    pub telegram: Option<TelegramChannel>,
}

// ---------------------------------------------------------------------------
// Real build
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-network"))]
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    use std::time::Duration;

    use crate::clients::chat_completion::CompletionsClient;
    use crate::clients::telegram::TelegramClient;
    use crate::secrets::load_secret;
    use crate::tools::{github, network};

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

    let provider_api_key = load_secret("provider-api-key").unwrap_or_else(|e| {
        error!("Failed to load Provider credentials: {e}");
        std::process::exit(1);
    });

    let client =
        CompletionsClient::new(config.provider.api.endpoint().to_string(), provider_api_key);
    let provider = CompletionsProvider::new(client.clone(), &config.provider);

    let mut tools = Tools::local(workspace, config);
    tools.extend(github::build(github_token, workspace, config));
    tools.extend(network::build(config, client));

    let telegram = telegram_token.map(|token| {
        let tg_client = TelegramClient::new(
            token,
            Duration::from_secs(config.telegram.poll_timeout_secs + 10),
        );
        TelegramChannel::new(tg_client, config.telegram.chat_id)
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
    use std::time::Duration;

    use crate::clients::chat_completion::CompletionsClient;
    use crate::clients::telegram::TelegramClient;
    use crate::secrets::{Secret, load_secret};
    use crate::tools::github;

    let client = CompletionsClient::new(
        config.provider.api.endpoint().to_string(),
        Secret::placeholder(),
    );
    let provider = CompletionsProvider::new(client, &config.provider);

    let github_token = if config.github.enabled {
        load_secret("github-token").ok()
    } else {
        None
    };

    let mut tools = Tools::local(workspace, config);
    tools.extend(github::build(github_token, workspace, config));

    let telegram = if config.telegram.enabled {
        Some(TelegramChannel::new(
            TelegramClient::new(
                Secret::placeholder(),
                Duration::from_secs(config.telegram.poll_timeout_secs + 10),
            ),
            config.telegram.chat_id,
        ))
    } else {
        None
    };

    Runtime {
        provider,
        tools: Tools::new(tools, &config.tools.disabled).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(1);
        }),
        telegram,
    }
}
