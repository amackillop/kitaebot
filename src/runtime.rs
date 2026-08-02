//! Application runtime — assembles provider, tools, and channels.
//!
//! The only `mock-network` differences live here and in the clients:
//! secrets become placeholders and the network tools are compiled out.
//! Everything else builds identically, so tests exercise the real
//! construction path against loopback fixture servers.

use std::sync::Arc;
use std::time::Duration;

use tracing::error;

use crate::channel::linear::LinearChannel;
use crate::channel::telegram::TelegramChannel;
use crate::clients::chat_completion::CompletionsClient;
use crate::clients::github::GithubClient;
use crate::clients::linear::LinearClient;
use crate::clients::telegram::TelegramClient;
use crate::config::Config;
use crate::notify::{Notifier, NotifyTool};
use crate::provider::CompletionsProvider;
use crate::secrets::Secret;
use crate::tools::git::GitCli;
use crate::tools::github::GhCli;
use crate::tools::{DirenvCache, Tool, Tools, Warmer, git, github, linear};
use crate::workspace::Workspace;

/// Notifier wired to its durable mirror (spec 17).
fn build_notifier(
    client: &TelegramClient,
    config: &Config,
    workspace: &Workspace,
) -> Arc<Notifier> {
    Arc::new(
        Notifier::new(client.clone(), config.telegram.chat_id).with_log(workspace.journal_path()),
    )
}

/// Load a secret by name, exiting on failure.
#[cfg(not(feature = "mock-network"))]
fn secret(name: &str) -> Secret {
    crate::secrets::load_secret(name).unwrap_or_else(|e| {
        error!("Failed to load secret {name}: {e}");
        std::process::exit(1);
    })
}

/// Placeholder secret — fixture servers don't check credentials.
#[cfg(feature = "mock-network")]
fn secret(_name: &str) -> Secret {
    Secret::placeholder()
}

/// Fully-assembled application runtime returned by [`build`].
pub struct Runtime {
    pub provider: CompletionsProvider,
    pub tools: Tools,
    pub telegram: Option<TelegramChannel>,
    pub notifier: Option<Arc<Notifier>>,
    /// REST client for the GitHub channel; `Some` iff `github.enabled`.
    pub github: Option<GithubClient>,
    /// Used by the GitHub channel to prepare review checkouts and by
    /// the duty scheduler to warm build caches.
    pub git_cli: Option<GitCli>,
    pub linear: Option<LinearChannel>,
}

pub fn build(config: &Config, workspace: &Workspace) -> Runtime {
    let direnv_cache = DirenvCache::new();
    let warmer = Warmer::new(direnv_cache.clone());
    let mut tools = Tools::local(workspace, config, direnv_cache.clone());

    let (github, git_cli) = build_git(config, workspace, &mut tools, &direnv_cache, &warmer);

    let linear = if config.linear.enabled {
        let client = LinearClient::new(secret("linear-api-key"), &config.linear.api_base);
        tools.extend(linear::build(client.clone()));
        Some(LinearChannel::new(
            client,
            Duration::from_secs(config.linear.poll_interval_secs),
            config.linear.trusted_users.clone(),
            git_cli.clone(),
        ))
    } else {
        None
    };

    let client = CompletionsClient::new(
        config.provider.api.endpoint().to_string(),
        secret("provider-api-key"),
    );
    let provider = CompletionsProvider::new(client.clone(), &config.provider);
    #[cfg(not(feature = "mock-network"))]
    tools.extend(crate::tools::network::build(config, client));

    let (telegram, notifier) = if config.telegram.enabled {
        let tg_client = TelegramClient::new(
            secret("telegram-bot-token"),
            Duration::from_secs(config.telegram.poll_timeout_secs + 10),
            &config.telegram.api_base,
        );
        let notifier = build_notifier(&tg_client, config, workspace);
        tools.push(Arc::new(NotifyTool(notifier.clone())));
        (
            Some(TelegramChannel::new(tg_client, config.telegram.chat_id)),
            Some(notifier),
        )
    } else {
        (None, None)
    };

    Runtime {
        provider,
        tools: Tools::new(tools, &config.tools.disabled).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(1);
        }),
        telegram,
        notifier,
        github,
        git_cli,
        linear,
    }
}

/// Git tools, the git CLI, and the GitHub REST client, when a GitHub
/// token is configured.
fn build_git(
    config: &Config,
    workspace: &Workspace,
    tools: &mut Vec<Arc<dyn Tool>>,
    direnv_cache: &DirenvCache,
    warmer: &Warmer,
) -> (Option<GithubClient>, Option<GitCli>) {
    if !(config.git.enabled || config.github.enabled) {
        return (None, None);
    }
    let token = secret("github-token");
    if config.git.enabled {
        tools.extend(git::build(
            token.clone(),
            workspace,
            &config.git,
            direnv_cache.clone(),
            warmer.clone(),
        ));
    }
    // Built whenever a token exists: the GitHub channel prepares
    // review checkouts with it (gated on github.enabled) and the
    // duty scheduler warms build caches with it.
    let git_cli = GitCli::new(
        token.clone(),
        workspace.path(),
        direnv_cache.clone(),
        config.git.trusted_repos(),
    )
    .with_warm(warmer.clone(), Arc::new(config.git.warm_commands()))
    .with_clone_base(&config.git.clone_base);
    let github_client = if config.github.enabled {
        let client = GithubClient::new(token.clone(), &config.github.api_base);
        tools.extend(github::build(
            GhCli::new(token, workspace.path()),
            github::GithubApi::new(client.clone(), workspace.path()),
        ));
        Some(client)
    } else {
        None
    };
    (github_client, Some(git_cli))
}
