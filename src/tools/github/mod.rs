//! GitHub integration tools.
//!
//! Provides authenticated GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GH_TOKEN` (for `gh`).
//!
//! # Architecture
//!
//! [`crate::tools::cli_runner::exec`] is the subprocess boundary.
//! [`crate::tools::git::GitCli`] wraps the `git` binary (clone, push, commit).
//! [`gh_cli::GhCli`] wraps the `gh` CLI (PRs, CI, API calls).
//! Each tool owns a clone of the appropriate CLI struct and holds
//! only its business logic.
//!
//! Tools expose a `prepare()` method that returns a
//! [`crate::tools::cli_runner::SubprocessCall`] — a pure value
//! describing what to run. Tests check this value directly without
//! spawning subprocesses.
//!
//! # Token injection
//!
//! For `gh` commands, `GH_TOKEN` is injected into the subprocess
//! environment. For `git clone`/`push`, a temporary `GIT_ASKPASS`
//! script is used — see [`crate::tools::git`].

mod ci_status;
mod gh;
mod gh_cli;
mod pr_create;
mod pr_diff_comments;
mod pr_diff_reply;
mod pr_list;
mod pr_review;
mod pr_reviews;
#[cfg(test)]
mod test_helpers;
mod types;

pub use ci_status::CiStatus;
pub use gh::Gh;
pub use gh_cli::GhCli;
pub use pr_create::PrCreate;
pub use pr_diff_comments::PrDiffComments;
pub use pr_diff_reply::PrDiffReply;
pub use pr_list::PrList;
pub use pr_review::PrReview;
pub use pr_reviews::PrReviews;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::{Tool, ToolCtx};

use std::path::PathBuf;
use std::sync::Arc;

use crate::clients::github::GithubClient;
use crate::error::ToolError;

/// Shared context for REST-backed GitHub tools: the API client plus
/// repo-dir resolution against the workspace. The `owner/repo` a tool
/// acts on comes from the checkout's origin remote — the model names
/// a directory, never a repo.
#[derive(Clone)]
pub struct GithubApi {
    client: GithubClient,
    workspace_root: PathBuf,
}

impl GithubApi {
    pub fn new(client: GithubClient, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            client,
            workspace_root: workspace_root.into(),
        }
    }

    fn client(&self) -> &GithubClient {
        &self.client
    }

    /// Resolve a workspace-relative repo dir to its origin `owner/repo`.
    async fn nwo(&self, repo_dir: &str) -> Result<String, ToolError> {
        let dir = crate::tools::git::resolve_repo_dir(&self.workspace_root, repo_dir)?;
        crate::tools::git::origin_nwo(&dir).await.ok_or_else(|| {
            ToolError::InvalidArguments(format!("cannot resolve origin owner/repo for {repo_dir}"))
        })
    }
}

/// Map a client error into the tool error surface.
fn api_err(e: &crate::error::GithubError) -> ToolError {
    ToolError::ExecutionFailed(e.to_string())
}

/// Build the GitHub tools.
pub(crate) fn build(gh: GhCli, api: GithubApi) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CiStatus(gh.clone())),
        Arc::new(Gh(gh.clone())),
        Arc::new(PrCreate(gh.clone())),
        Arc::new(PrDiffComments(api.clone())),
        Arc::new(PrDiffReply(api.clone())),
        Arc::new(PrList(gh)),
        Arc::new(PrReview(api.clone())),
        Arc::new(PrReviews(api)),
    ]
}
