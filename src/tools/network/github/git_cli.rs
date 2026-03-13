//! `git` subprocess wrapper.
//!
//! [`GitCli`] owns the token and workspace root needed by git tools
//! (clone, push, commit). Auth uses a temporary `GIT_ASKPASS` script
//! written to a private directory for the duration of one command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{CliRunner, CmdOutput};

/// Shared context for git tools.
///
/// Generic over [`CliRunner`] so tests can substitute a stub without
/// spawning real subprocesses.
pub struct GitCli<R> {
    pub(super) runner: R,
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
    pub(super) co_authors: Vec<String>,
}

impl<R: CliRunner> GitCli<R> {
    pub fn new(
        runner: R,
        token: Secret,
        workspace_root: impl Into<PathBuf>,
        co_authors: Vec<String>,
    ) -> Self {
        Self {
            runner,
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

    /// Get the current branch name from a git working directory.
    pub async fn current_branch(&self, cwd: &Path) -> Result<String, ToolError> {
        let output = self
            .run_git_raw(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, false)
            .await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "failed to get current branch: {}",
                output.stderr
            )));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Run a git command, format output as envelope for the LLM.
    pub async fn run_git(
        &self,
        args: &[&str],
        cwd: &Path,
        authenticated: bool,
    ) -> Result<String, ToolError> {
        self.run_git_raw(args, cwd, authenticated).await?.format()
    }

    // ── Private helpers ─────────────────────────────────────────────

    /// Run `git` with optional credential injection.
    async fn run_git_raw(
        &self,
        args: &[&str],
        cwd: &Path,
        authenticated: bool,
    ) -> Result<CmdOutput, ToolError> {
        let askpass = if authenticated {
            Some(AskPass::create(&self.token).await?)
        } else {
            None
        };

        let mut env: Vec<(OsString, OsString)> = crate::tools::safe_env().collect();

        if let Some(ref ap) = askpass {
            env.push(("GIT_ASKPASS".into(), ap.path().as_os_str().to_owned()));
            env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
        }

        let output = self.runner.exec("git", args, cwd, &env).await;
        drop(askpass);
        output
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
    use super::super::test_helpers::{err_output, ok_output, stub_git_cli_with_repo};
    use crate::error::ToolError;

    #[tokio::test]
    async fn current_branch_trims_output() {
        let (cli, repo) = stub_git_cli_with_repo(vec![ok_output("  feature/foo\n")]);
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let branch = cli.current_branch(&cwd).await.unwrap();
        assert_eq!(branch, "feature/foo");
    }

    #[tokio::test]
    async fn current_branch_nonzero_exit() {
        let (cli, repo) = stub_git_cli_with_repo(vec![err_output("not a git repo")]);
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let result = cli.current_branch(&cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("current branch"))
        );
    }
}
