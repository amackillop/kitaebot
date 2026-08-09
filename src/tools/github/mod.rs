//! GitHub integration tools.
//!
//! Every tool talks to the REST API through the in-process
//! [`GithubClient`] via the shared [`GithubApi`] context. The token
//! never reaches the exec tool: it lives inside the client's IO
//! closure (and in `GIT_ASKPASS` for `git` — see
//! [`crate::tools::git`]).

mod api;
mod ci_status;
mod comment_update;
mod issue_create;
mod pr_create;
mod pr_diff_comments;
mod pr_diff_reply;
mod pr_list;
mod pr_review;
mod pr_reviews;
#[cfg(test)]
mod test_helpers;

pub use api::Api;
pub use ci_status::CiStatus;
pub use comment_update::CommentUpdate;
pub use issue_create::IssueCreate;
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

    /// Resolve and validate a repo directory within the workspace.
    fn dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        crate::tools::git::resolve_repo_dir(&self.workspace_root, repo_dir)
    }

    /// Resolve a workspace-relative repo dir to its origin `owner/repo`.
    async fn nwo(&self, repo_dir: &str) -> Result<String, ToolError> {
        let dir = self.dir(repo_dir)?;
        crate::tools::git::origin_nwo(&dir).await.ok_or_else(|| {
            ToolError::InvalidArguments(format!("cannot resolve origin owner/repo for {repo_dir}"))
        })
    }
}

/// The currently checked-out branch of a git working directory.
async fn current_branch(cwd: &std::path::Path) -> Result<String, ToolError> {
    let call = crate::tools::cli_runner::SubprocessCall {
        binary: "git",
        args: vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        cwd: cwd.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: None,
        stdin: None,
        // Fixed argv, runs no hooks, reads one ref name.
        confine: None,
    };
    let output = crate::tools::cli_runner::exec(&call).await?;
    if output.exit_code != 0 {
        return Err(ToolError::ExecutionFailed(format!(
            "failed to get current branch: {}",
            output.stderr
        )));
    }
    Ok(output.stdout.trim().to_string())
}

/// Build the GitHub tools. `repos` bounds where issues can be filed —
/// the `[git.repositories]` keys.
pub(crate) fn build(api: GithubApi, repos: Vec<String>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Api(api.clone())),
        Arc::new(CiStatus(api.clone())),
        Arc::new(CommentUpdate {
            api: api.clone(),
            repos: repos.clone(),
        }),
        Arc::new(IssueCreate {
            api: api.clone(),
            repos,
        }),
        Arc::new(PrCreate(api.clone())),
        Arc::new(PrDiffComments(api.clone())),
        Arc::new(PrDiffReply(api.clone())),
        Arc::new(PrList(api.clone())),
        Arc::new(PrReview(api.clone())),
        Arc::new(PrReviews(api)),
    ]
}
