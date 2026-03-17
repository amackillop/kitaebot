//! Git operations tools.
//!
//! Pure git tools (clone, push, commit) that require an authentication
//! token but no GitHub-specific CLI. Auth uses a temporary `GIT_ASKPASS`
//! script injected for the duration of one command.

mod commit;
pub(crate) mod git_cli;
mod git_clone;
mod push;
#[cfg(test)]
pub(super) mod test_helpers;
pub(crate) mod url;

pub use commit::Commit;
pub use git_cli::GitCli;
pub use git_clone::GitClone;
pub use push::Push;

use std::path::{Path, PathBuf};

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::DirenvCache;
use crate::workspace::Workspace;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::Tool;

/// Resolve and validate a repo directory within the workspace.
///
/// Rejects path traversal (`..`), absolute paths, paths that escape
/// the workspace root, and directories without a `.git` subdirectory.
pub(crate) fn resolve_repo_dir(
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

/// Build the git tools. Returns an empty vec when no token is provided.
pub(crate) fn build(
    token: Secret,
    workspace: &Workspace,
    co_authors: Vec<String>,
    direnv: DirenvCache,
) -> Vec<Box<dyn Tool>> {
    let git = GitCli::new(token, workspace.path(), direnv.clone());

    vec![
        Box::new(Commit::new(git.clone(), co_authors)),
        Box::new(Push(git.clone())),
        Box::new(GitClone(git, direnv)),
    ]
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
