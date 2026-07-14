//! Git operations tools.
//!
//! Pure git tools (clone, push, commit) that require an authentication
//! token but no GitHub-specific CLI. Auth uses a temporary `GIT_ASKPASS`
//! script injected for the duration of one command.

pub(crate) mod checkout;
mod commit;
mod fetch;
pub(crate) mod git_cli;
mod git_clone;
mod push;
#[cfg(test)]
pub(super) mod test_helpers;
pub(crate) mod url;

pub use commit::Commit;
pub use fetch::Fetch;
pub use git_cli::GitCli;
pub use git_clone::GitClone;
pub use push::Push;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::DirenvCache;
use crate::workspace::Workspace;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::{Tool, ToolCtx};

/// Resolve and validate a repo directory within the workspace.
///
/// Rejects path traversal (`..`), absolute paths, paths that escape
/// the workspace root, and directories without a `.git` subdirectory.
pub(crate) fn resolve_repo_dir(
    workspace_root: &Path,
    repo_dir: &str,
) -> Result<PathBuf, ToolError> {
    if repo_dir.contains("..") {
        return Err(ToolError::Blocked {
            operation: repo_dir.to_string(),
            guidance: "repo_dir: path traversal detected".into(),
        });
    }
    if Path::new(repo_dir).is_absolute() {
        return Err(ToolError::Blocked {
            operation: repo_dir.to_string(),
            guidance: "repo_dir: absolute paths not allowed".into(),
        });
    }

    let resolved = workspace_root.join(repo_dir);
    if !resolved.starts_with(workspace_root) {
        return Err(ToolError::Blocked {
            operation: repo_dir.to_string(),
            guidance: "repo_dir: escapes workspace".into(),
        });
    }
    if !resolved.join(".git").is_dir() {
        return Err(ToolError::InvalidArguments(format!(
            "{repo_dir} is not a git repository"
        )));
    }

    Ok(resolved)
}

/// Whether the checkout at `dir` has an `origin` remote whose `owner/repo`
/// is in `trusted`. Reads `git config --get remote.origin.url`; an
/// unreadable or unparseable remote counts as untrusted.
///
/// Lets [`crate::tools::exec`] re-`direnv allow` a trusted repo's `.envrc`
/// after a pull rewrote it (trust is content-bound, so a pull silently
/// revokes it), without re-deriving the nwo from a clone URL it never saw.
pub(crate) async fn origin_trusted(dir: &Path, trusted: &[String]) -> bool {
    origin_nwo(dir)
        .await
        .is_some_and(|nwo| url::is_trusted_repo(&nwo, trusted))
}

async fn origin_nwo(dir: &Path) -> Option<String> {
    use crate::tools::cli_runner::{self, SubprocessCall};

    let call = SubprocessCall {
        binary: "git",
        args: vec!["config".into(), "--get".into(), "remote.origin.url".into()],
        cwd: dir.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: Some(10),
        stdin: None,
    };
    let out = cli_runner::exec(&call).await.ok()?;
    if out.exit_code != 0 {
        return None;
    }
    let https = url::to_https_url(out.stdout.trim()).ok()?;
    url::extract_nwo(&https)
}

/// Build the git tools. Returns an empty vec when no token is provided.
pub(crate) fn build(
    token: Secret,
    workspace: &Workspace,
    config: &crate::config::GitConfig,
    direnv: DirenvCache,
) -> Vec<Arc<dyn Tool>> {
    let git = GitCli::new(token, workspace.path(), direnv.clone());

    vec![
        Arc::new(Commit::new(git.clone(), config.co_authors.clone())),
        Arc::new(Push(git.clone())),
        Arc::new(Fetch(git.clone())),
        Arc::new(GitClone {
            git,
            direnv,
            trusted_repos: config.trusted_repos.clone(),
        }),
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
            Err(ToolError::Blocked { .. })
        ));
    }

    #[test]
    fn rejects_absolute() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_repo_dir(workspace.path(), "/etc"),
            Err(ToolError::Blocked { .. })
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
