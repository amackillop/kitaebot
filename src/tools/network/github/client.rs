//! Shared client context for GitHub tools.
//!
//! [`GitHubClient`] wraps a [`CliRunner`] impl, owns the GitHub token,
//! and carries workspace root + co-author config. It provides the
//! plumbing methods that all tool modules use: subprocess execution,
//! JSON parsing, repo dir validation, and branch detection.
//!
//! Auth is handled here — `run_gh` injects `GH_TOKEN`, `run_git` with
//! `authenticated: true` creates a temporary `GIT_ASKPASS` script.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{CliRunner, CmdOutput};

/// Shared context for all GitHub tools.
///
/// Generic over [`CliRunner`] so tests can substitute a stub without
/// spawning real subprocesses.
pub struct GitHubClient<R> {
    pub(super) runner: R,
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
    pub(super) co_authors: Vec<String>,
}

impl<R: CliRunner> GitHubClient<R> {
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

    /// Run a `gh` command, format output as envelope for the LLM.
    pub async fn run_gh(&self, args: &[&str], cwd: &Path) -> Result<String, ToolError> {
        self.run_gh_raw(args, cwd).await?.format()
    }

    /// Run `gh` with `--json <fields>` and deserialize the response.
    pub async fn run_gh_json<T: DeserializeOwned>(
        &self,
        args: &[&str],
        fields: &str,
        cwd: &Path,
    ) -> Result<T, ToolError> {
        let full_args: Vec<&str> = args.iter().copied().chain(["--json", fields]).collect();
        self.run_gh_parse(&full_args, cwd).await
    }

    /// Run `gh api` and deserialize the JSON response.
    pub async fn run_gh_api<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        cwd: &Path,
    ) -> Result<T, ToolError> {
        self.run_gh_parse(&["api", endpoint], cwd).await
    }

    /// Run `gh`, check exit code, and deserialize stdout as JSON.
    pub async fn run_gh_parse<T: DeserializeOwned>(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> Result<T, ToolError> {
        let output = self.run_gh_raw(args, cwd).await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "{}: {}",
                output.command, output.stderr
            )));
        }
        serde_json::from_str(&output.stdout)
            .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", output.command)))
    }

    // ── Private helpers ─────────────────────────────────────────────

    /// Run `gh` with token + prompt-disabled env.
    async fn run_gh_raw(&self, args: &[&str], cwd: &Path) -> Result<CmdOutput, ToolError> {
        let env: Vec<(OsString, OsString)> = crate::tools::safe_env()
            .chain([
                ("GH_TOKEN".into(), self.token.expose().into()),
                ("GH_PROMPT_DISABLED".into(), "1".into()),
                ("NO_COLOR".into(), "1".into()),
            ])
            .collect();
        self.runner.exec("gh", args, cwd, &env).await
    }

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
    use super::super::test_helpers::{err_output, ok_output, stub_client_with_repo};
    use crate::error::ToolError;

    // ── run_gh / run_gh_parse ────────────────────────────────────────

    #[tokio::test]
    async fn run_gh_nonzero_exit_returns_error() {
        let (client, repo) = stub_client_with_repo(vec![err_output("not found")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result = client.run_gh(&["pr", "view"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_malformed_json_returns_error() {
        let (client, repo) = stub_client_with_repo(vec![ok_output("not json")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            client.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_nonzero_exit_returns_stderr() {
        let (client, repo) = stub_client_with_repo(vec![err_output("permission denied")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            client.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("permission denied"))
        );
    }

    // ── current_branch ───────────────────────────────────────────────

    #[tokio::test]
    async fn current_branch_trims_output() {
        let (client, repo) = stub_client_with_repo(vec![ok_output("  feature/foo\n")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let branch = client.current_branch(&cwd).await.unwrap();
        assert_eq!(branch, "feature/foo");
    }

    #[tokio::test]
    async fn current_branch_nonzero_exit() {
        let (client, repo) = stub_client_with_repo(vec![err_output("not a git repo")]);
        let cwd = client.resolve_repo_dir(&repo).unwrap();
        let result = client.current_branch(&cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("current branch"))
        );
    }
}
