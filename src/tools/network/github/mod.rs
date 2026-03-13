//! GitHub integration tools.
//!
//! Provides authenticated git and GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GIT_ASKPASS` (for git) or `GH_TOKEN` (for `gh`).
//!
//! # Architecture
//!
//! [`crate::tools::cli_runner::CliRunner`] is the raw subprocess boundary
//! — a single `exec` method that spawns any binary with an explicit env.
//! [`client::GitHubClient`] carries the shared context (runner, token,
//! workspace root, co-authors) and provides plumbing methods (`run_gh`,
//! `run_git`, etc.). Each tool file holds an `Arc<GitHubClient<R>>` and
//! owns only its business logic.
//!
//! Tests substitute `StubCliRunner` to exercise the logic without
//! spawning real subprocesses.
//!
//! # Token injection
//!
//! For `git clone`/`push`, a temporary helper script is written to a
//! private directory, set as `GIT_ASKPASS`, and deleted immediately after
//! the subprocess exits. The script prints the token to stdout when
//! invoked by git. The token is on disk for the duration of one git
//! command only.

mod ci_status;
mod client;
mod commit;
mod git_cli;
mod git_clone;
mod pr_comment;
mod pr_create;
mod pr_diff_comments;
mod pr_diff_reply;
mod pr_list;
mod pr_reviews;
mod push;
#[cfg(test)]
mod test_helpers;
mod types;
mod url;

pub use ci_status::CiStatus;
pub use client::GitHubClient;
pub use commit::Commit;
pub use git_cli::GitCli;
pub use git_clone::GitClone;
pub use pr_comment::PrComment;
pub use pr_create::PrCreate;
pub use pr_diff_comments::PrDiffComments;
pub use pr_diff_reply::PrDiffReply;
pub use pr_list::PrList;
pub use pr_reviews::PrReviews;
pub use push::Push;

use std::path::{Path, PathBuf};

use crate::error::ToolError;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::Tool;

/// Resolve and validate a repo directory within the workspace.
///
/// Rejects path traversal (`..`), absolute paths, paths that escape
/// the workspace root, and directories without a `.git` subdirectory.
pub(super) fn resolve_repo_dir(
    workspace_root: &Path,
    repo_dir: &str,
) -> Result<PathBuf, ToolError> {
    if repo_dir.contains("..") {
        return Err(ToolError::Blocked(
            "repo_dir: path traversal detected".into(),
        ));
    }
    if Path::new(repo_dir).is_absolute() {
        return Err(ToolError::Blocked(
            "repo_dir: absolute paths not allowed".into(),
        ));
    }

    let resolved = workspace_root.join(repo_dir);
    if !resolved.starts_with(workspace_root) {
        return Err(ToolError::Blocked("repo_dir: escapes workspace".into()));
    }
    if !resolved.join(".git").is_dir() {
        return Err(ToolError::InvalidArguments(format!(
            "{repo_dir} is not a git repository"
        )));
    }

    Ok(resolved)
}

#[cfg(test)]
mod resolve_repo_dir_tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_repo_dir(workspace.path(), "../escape"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn rejects_absolute() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_repo_dir(workspace.path(), "/etc"),
            Err(ToolError::Blocked(_))
        ));
    }

    #[test]
    fn rejects_non_repo() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("projects/notrepo")).unwrap();
        assert!(matches!(
            resolve_repo_dir(workspace.path(), "projects/notrepo"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn accepts_valid_repo() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("projects/myrepo/.git")).unwrap();
        let resolved = resolve_repo_dir(workspace.path(), "projects/myrepo").unwrap();
        assert!(resolved.ends_with("projects/myrepo"));
    }
}
