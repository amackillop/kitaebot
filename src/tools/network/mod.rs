//! Tools that require network access.
//!
//! Gated behind `#[cfg(not(feature = "mock-network"))]` at the parent
//! module level so the entire subtree is excluded from stub builds.

mod github;
mod web_fetch;
mod web_search;

pub use github::{GitHub, GitHubClient, RealGitHubApi};
pub use web_fetch::WebFetch;
pub use web_search::WebSearch;

// Re-export parent utilities so child modules can use `super::`.
pub(crate) use super::{Tool, truncate_output};

use std::sync::Arc;

use tracing::error;

use crate::clients::chat_completion::CompletionsClient;
use crate::config::Config;
use crate::secrets::Secret;
use crate::workspace::Workspace;

/// Build the network tools. Returns boxed trait objects ready for
/// inclusion in the tool collection.
pub fn build(
    workspace: &Workspace,
    config: &Config,
    client: CompletionsClient,
    github_token: Option<Secret>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    if let Some(token) = github_token {
        let api = RealGitHubApi::new(token);
        let gh = Arc::new(GitHubClient::new(
            api,
            workspace.path(),
            config.git.co_authors.clone(),
        ));
        tools.push(Box::new(GitHub::new(Arc::clone(&gh))));
        tools.push(Box::new(github::CiStatus(Arc::clone(&gh))));
        tools.push(Box::new(github::Commit(Arc::clone(&gh))));
        tools.push(Box::new(github::GitClone(Arc::clone(&gh))));
        tools.push(Box::new(github::PrComment(gh)));
    }

    tools.push(Box::new(
        WebFetch::new(&config.tools.web_fetch).unwrap_or_else(|e| {
            error!("Failed to initialize web_fetch: {e}");
            std::process::exit(1);
        }),
    ));

    tools.push(Box::new(WebSearch::new(client, &config.tools.web_search)));

    tools
}
