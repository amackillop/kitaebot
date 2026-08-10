//! Git operations tools.
//!
//! Pure git tools (clone, push, commit) that require an authentication
//! token but no GitHub-specific CLI. Auth uses a temporary `GIT_ASKPASS`
//! script injected for the duration of one command.

pub(crate) mod checkout;
mod commit;
mod fetch;
mod fixup;
pub(crate) mod git_cli;
mod git_clone;
mod push;
mod rebase;
#[cfg(test)]
pub(super) mod test_helpers;
pub(crate) mod url;

pub use commit::Commit;
pub use fetch::Fetch;
pub use fixup::Fixup;
pub use git_cli::GitCli;
pub use git_clone::GitClone;
pub use push::Push;
pub use rebase::Rebase;

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
/// the workspace root, and directories without a `.git` entry.
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
    // A worktree's .git is a file pointing at its clone; review
    // checkouts are worktrees (spec 20).
    let marker = resolved.join(".git");
    if !marker.is_dir() && !marker.is_file() {
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
/// Gates every trust decision made after a clone exists — exec's
/// re-`direnv allow` and [`GitCli::warm_devshell`] — so no caller ever
/// re-derives the nwo from a clone URL it never saw.
pub(crate) async fn origin_trusted(dir: &Path, trusted: &[String]) -> bool {
    origin_nwo(dir)
        .await
        .is_some_and(|nwo| url::is_trusted_repo(&nwo, trusted))
}

pub(crate) async fn origin_nwo(dir: &Path) -> Option<String> {
    use crate::tools::cli_runner::{self, SubprocessCall};

    let call = SubprocessCall {
        binary: "git",
        args: vec!["config".into(), "--get".into(), "remote.origin.url".into()],
        cwd: dir.to_path_buf(),
        env: crate::tools::safe_env().collect(),
        timeout_secs: Some(10),
        stdin: None,
        // Fixed argv, runs no hooks, reads one config value.
        confine: None,
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
    warmer: crate::tools::Warmer,
    confine: bool,
) -> Vec<Arc<dyn Tool>> {
    let git = GitCli::new(token, workspace.path(), direnv, config.trusted_repos())
        .with_warm(warmer, Arc::new(config.warm_commands()))
        .with_clone_base(&config.clone_base)
        .with_confinement(confine);

    vec![
        Arc::new(Commit::new(git.clone(), config.co_authors.clone())),
        Arc::new(Fixup(git.clone())),
        Arc::new(Push(git.clone())),
        Arc::new(Fetch(git.clone())),
        Arc::new(Rebase(git.clone())),
        Arc::new(GitClone { git }),
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

    #[test]
    fn accepts_worktree_with_gitdir_file() {
        let workspace = tempfile::tempdir().unwrap();
        let wt = workspace.path().join("reviews/o/r");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: ../../../projects/o/r/.git/worktrees/r\n",
        )
        .unwrap();
        let resolved = resolve_repo_dir(workspace.path(), "reviews/o/r").unwrap();
        assert!(resolved.ends_with("reviews/o/r"));
    }
}
