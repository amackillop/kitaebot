//! `gh` CLI subprocess wrapper.
//!
//! [`GhCli`] owns the token and workspace root needed by GitHub CLI
//! tools (PRs, CI status, API calls). Auth injects `GH_TOKEN` into
//! the subprocess environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::{CliRunner, CmdOutput};

/// Shared context for `gh` CLI tools.
///
/// Generic over [`CliRunner`] so tests can substitute a stub without
/// spawning real subprocesses.
pub struct GhCli<R> {
    pub(super) runner: R,
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
}

impl<R: CliRunner> GhCli<R> {
    pub fn new(runner: R, token: Secret, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            token,
            workspace_root: workspace_root.into(),
        }
    }

    /// Resolve and validate a repo directory within the workspace.
    pub fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        super::resolve_repo_dir(&self.workspace_root, repo_dir)
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
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{err_output, ok_output, stub_gh_cli_with_repo};
    use crate::error::ToolError;

    #[tokio::test]
    async fn run_gh_nonzero_exit_returns_error() {
        let (cli, repo) = stub_gh_cli_with_repo(vec![err_output("not found")]);
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let result = cli.run_gh(&["pr", "view"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_malformed_json_returns_error() {
        let (cli, repo) = stub_gh_cli_with_repo(vec![ok_output("not json")]);
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            cli.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn run_gh_parse_nonzero_exit_returns_stderr() {
        let (cli, repo) = stub_gh_cli_with_repo(vec![err_output("permission denied")]);
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let result: Result<Vec<serde_json::Value>, _> =
            cli.run_gh_parse(&["pr", "list"], &cwd).await;
        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(msg)) if msg.contains("permission denied"))
        );
    }
}
