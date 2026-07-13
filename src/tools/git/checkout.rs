//! Shared helpers for channel-prepared checkouts.
//!
//! Both the GitHub review path (`reviews/<owner>/<repo>`) and the
//! Linear execution path (`projects/<owner>/<repo>`) clone a repo into
//! a per-repo directory, then position HEAD before a turn runs. The
//! cloning and path plumbing are identical; only the refs each fetches
//! and where it parks HEAD differ.

use std::path::{Path, PathBuf};

use super::GitCli;
use super::url::validate_name;
use crate::error::ToolError;

/// Workspace-relative checkout dir `<root>/<owner>/<repo>`.
pub(crate) fn rel_path(root: &str, nwo: &str) -> Result<String, ToolError> {
    let (owner, repo) = nwo
        .split_once('/')
        .ok_or_else(|| ToolError::InvalidArguments(format!("expected owner/repo, got: {nwo}")))?;
    Ok(format!(
        "{root}/{}/{}",
        validate_name(owner)?,
        validate_name(repo)?
    ))
}

/// Clone `url` into the workspace-relative `rel` if it is not already a
/// git repo. Returns the absolute checkout directory.
pub(crate) async fn ensure_cloned(
    git: &GitCli,
    url: &str,
    rel: &str,
) -> Result<PathBuf, ToolError> {
    let dir = git.workspace_root().join(rel);
    if !dir.join(".git").is_dir() {
        let parent = dir.parent().expect("rel path has parent components");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("create {}: {e}", parent.display())))?;
        let name = rel.rsplit('/').next().expect("rsplit yields at least one");
        run(git, &["clone", url, name], parent, true).await?;
    }
    Ok(dir)
}

/// Run git in `cwd`, discarding output, with optional credential injection.
pub(crate) async fn run(
    git: &GitCli,
    args: &[&str],
    cwd: &Path,
    authenticated: bool,
) -> Result<(), ToolError> {
    let call = git.prepare_git(args, cwd);
    git.exec_git(call, authenticated).await?.format().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_joins_owner_and_repo_under_root() {
        assert_eq!(
            rel_path("reviews", "owner/repo").unwrap(),
            "reviews/owner/repo"
        );
        assert_eq!(
            rel_path("projects", "owner/repo").unwrap(),
            "projects/owner/repo"
        );
    }

    #[test]
    fn rel_path_rejects_missing_slash() {
        assert!(rel_path("reviews", "just-a-repo").is_err());
    }

    #[test]
    fn rel_path_rejects_extra_segments() {
        assert!(rel_path("reviews", "a/b/c").is_err());
    }

    #[test]
    fn rel_path_rejects_traversal() {
        assert!(rel_path("reviews", "../escape").is_err());
        assert!(rel_path("reviews", "owner/..").is_err());
    }

    #[test]
    fn rel_path_rejects_option_shaped_segments() {
        assert!(rel_path("reviews", "-flag/repo").is_err());
        assert!(rel_path("reviews", "owner/-flag").is_err());
    }
}
