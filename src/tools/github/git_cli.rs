//! `git` subprocess wrapper.
//!
//! [`GitCli`] owns the token and workspace root needed by git tools
//! (clone, push, commit). Auth uses a temporary `GIT_ASKPASS` script
//! written to a private directory for the duration of one command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{self, CmdOutput, SubprocessCall};

/// Shared context for git tools.
#[derive(Clone)]
pub struct GitCli {
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
    pub(super) co_authors: Vec<String>,
}

impl GitCli {
    pub fn new(token: Secret, workspace_root: impl Into<PathBuf>, co_authors: Vec<String>) -> Self {
        Self {
            token,
            workspace_root: workspace_root.into(),
            co_authors,
        }
    }

    /// Resolve and validate a repo directory within the workspace.
    pub fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        super::resolve_repo_dir(&self.workspace_root, repo_dir)
    }

    /// Workspace root path. Used by `GitClone` to locate the
    /// `projects/` directory.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Co-author trailers appended to commit messages.
    pub fn co_authors(&self) -> &[String] {
        &self.co_authors
    }

    /// Build a [`SubprocessCall`] for `git` without executing it.
    ///
    /// The returned call does **not** include `GIT_ASKPASS` — that is
    /// an effect created at execution time by [`Self::exec_git`].
    #[allow(clippy::unused_self)] // method for API consistency with prepare_gh
    pub fn prepare_git(&self, args: &[&str], cwd: &Path) -> SubprocessCall {
        let env: Vec<(OsString, OsString)> = crate::tools::safe_env().collect();
        SubprocessCall {
            binary: "git",
            args: args.iter().map(ToString::to_string).collect(),
            cwd: cwd.to_path_buf(),
            env,
        }
    }

    /// Execute a [`SubprocessCall`] with optional credential injection.
    ///
    /// When `authenticated` is true, a temporary `GIT_ASKPASS` script
    /// is created, added to the call's env, and deleted after execution.
    pub async fn exec_git(
        &self,
        mut call: SubprocessCall,
        authenticated: bool,
    ) -> Result<CmdOutput, ToolError> {
        let askpass = if authenticated {
            Some(AskPass::create(&self.token).await?)
        } else {
            None
        };

        if let Some(ref ap) = askpass {
            call.env
                .push(("GIT_ASKPASS".into(), ap.path().as_os_str().to_owned()));
            call.env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
        }

        let output = cli_runner::exec(&call).await;
        drop(askpass);
        output
    }

    /// Get the current branch name from a git working directory.
    pub async fn current_branch(&self, cwd: &Path) -> Result<String, ToolError> {
        let call = self.prepare_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd);
        let output = self.exec_git(call, false).await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "failed to get current branch: {}",
                output.stderr
            )));
        }
        Ok(output.stdout.trim().to_string())
    }
}

// ── GIT_ASKPASS helper ──────────────────────────────────────────────

/// A temporary `GIT_ASKPASS` script that prints the token.
///
/// The script lives in a private temp directory (mode 0700). The
/// directory is owned by a `TempDir` and removed on drop, so cleanup
/// happens even if the git command fails or the future is cancelled.
struct AskPass {
    /// Path to the executable script inside `_dir`.
    path: PathBuf,
    /// Owns the temp directory. Removed on drop.
    _dir: tempfile::TempDir,
}

impl AskPass {
    async fn create(token: &Secret) -> Result<Self, ToolError> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kitaebot-askpass-")
            .tempdir()
            .map_err(|e| ToolError::ExecutionFailed(format!("tmpdir: {e}")))?;

        let path = dir.path().join("askpass");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", token.expose());

        tokio::fs::write(&path, &script)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("write askpass: {e}")))?;

        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("chmod askpass: {e}")))?;

        Ok(Self { path, _dir: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::github::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn prepare_git_builds_correct_call() {
        let (cli, repo) = stub_git_cli_with_repo();
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let call = cli.prepare_git(&["rev-parse", "--abbrev-ref", "HEAD"], &cwd);
        assert_eq!(call.binary, "git");
        assert_eq!(call.args, ["rev-parse", "--abbrev-ref", "HEAD"]);
        assert!(!call.has_env("GIT_ASKPASS"));
    }
}
