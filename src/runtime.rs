//! Application runtime — assembles provider, tools, and channels.
//!
//! All `mock-network` conditional compilation for construction lives here,
//! keeping the rest of the codebase cfg-free.

use tracing::error;

use crate::config::Config;
use crate::linear_channel::LinearChannel;
use crate::provider::CompletionsProvider;
use crate::telegram::TelegramChannel;
use crate::tools::Tools;
use crate::tools::github::GhCli;
use crate::workspace::Workspace;

/// Fully-assembled application runtime returned by [`build`].
pub struct Runtime {
    pub provider: CompletionsProvider,
    pub tools: Tools,
    pub telegram: Option<TelegramChannel>,
    pub gh_cli: Option<GhCli>,
    pub linear: Option<LinearChannel>,
}

// ---------------------------------------------------------------------------
// Real build
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-network"))]
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    use std::time::Duration;

    use crate::clients::chat_completion::CompletionsClient;
    use crate::clients::linear::LinearClient;
    use crate::clients::telegram::TelegramClient;
    use crate::secrets::load_secret;
    use crate::tools::{DirenvCache, git, github, network};

    let direnv_cache = DirenvCache::new();
    let mut tools = Tools::local(workspace, config, direnv_cache.clone());

    let telegram_token = if config.telegram.enabled {
        Some(load_secret("telegram-bot-token").unwrap_or_else(|e| {
            error!("Failed to load Telegram credentials: {e}");
            std::process::exit(1);
        }))
    } else {
        None
    };
    let gh_cli = if config.git.enabled || config.github.enabled {
        let token = load_secret("github-token").unwrap_or_else(|e| {
            error!("Failed to load GitHub token: {e}");
            std::process::exit(1);
        });
        if config.git.enabled {
            tools.extend(git::build(
                token.clone(),
                workspace,
                config.git.co_authors.clone(),
                direnv_cache.clone(),
            ));
        }
        let gh = GhCli::new(token, workspace.path());
        if config.github.enabled {
            tools.extend(github::build(gh.clone()));
        }
        Some(gh)
    } else {
        None
    };

    let linear = if config.linear.enabled {
        let key = load_secret("linear-api-key").unwrap_or_else(|e| {
            error!("Failed to load Linear credentials: {e}");
            std::process::exit(1);
        });
        Some(LinearChannel::new(
            LinearClient::new(key),
            Duration::from_secs(config.linear.poll_interval_secs),
            config.linear.trusted_users.clone(),
        ))
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
        gh_cli,
        linear,
    }
}

// ---------------------------------------------------------------------------
// Stub build (mock-network)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-network")]
pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    use std::time::Duration;

    use crate::clients::chat_completion::CompletionsClient;
    use crate::clients::linear::LinearClient;
    use crate::clients::telegram::TelegramClient;
    use crate::secrets::{Secret, load_secret};
    use crate::tools::{DirenvCache, git, github};

    let client = CompletionsClient::new(
        config.provider.api.endpoint().to_string(),
        Secret::placeholder(),
    );
    let provider = CompletionsProvider::new(client, &config.provider);

    let direnv_cache = DirenvCache::new();
    let mut tools = Tools::local(workspace, config, direnv_cache.clone());
    let gh_cli = if config.git.enabled || config.github.enabled {
        let token = load_secret("github-token").unwrap_or_else(|e| {
            error!("Failed to load GitHub token: {e}");
            std::process::exit(1);
        });
        if config.git.enabled {
            tools.extend(git::build(
                token.clone(),
                workspace,
                config.git.co_authors.clone(),
                direnv_cache.clone(),
            ));
        }
        let gh = GhCli::new(token, workspace.path());
        if config.github.enabled {
            tools.extend(github::build(gh.clone()));
        }
        Some(gh)
    } else {
        None
    };

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

    let linear = if config.linear.enabled {
        Some(LinearChannel::new(
            LinearClient::new(Secret::placeholder()),
            Duration::from_secs(config.linear.poll_interval_secs),
            config.linear.trusted_users.clone(),
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
        gh_cli,
        linear,
    }
}
