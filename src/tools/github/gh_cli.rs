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
use crate::tools::cli_runner::{self, SubprocessCall};

/// Shared context for `gh` CLI tools.
#[derive(Clone)]
pub struct GhCli {
    pub(super) token: Secret,
    pub(super) workspace_root: PathBuf,
}

impl GhCli {
    pub fn new(token: Secret, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            token,
            workspace_root: workspace_root.into(),
        }
    }

    /// Root directory of the workspace (used as cwd for non-repo commands).
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Resolve and validate a repo directory within the workspace.
    pub fn resolve_repo_dir(&self, repo_dir: &str) -> Result<PathBuf, ToolError> {
        crate::tools::git::resolve_repo_dir(&self.workspace_root, repo_dir)
    }

    /// Build a [`SubprocessCall`] for `gh` without executing it.
    pub fn prepare_gh(&self, args: &[&str], cwd: &Path) -> SubprocessCall {
        let env: Vec<(OsString, OsString)> = crate::tools::safe_env()
            .chain([
                ("GH_TOKEN".into(), self.token.expose().into()),
                ("GH_PROMPT_DISABLED".into(), "1".into()),
                ("NO_COLOR".into(), "1".into()),
            ])
            .collect();
        SubprocessCall {
            binary: "gh",
            args: args.iter().map(ToString::to_string).collect(),
            cwd: cwd.to_path_buf(),
            env,
            timeout_secs: None,
        }
    }

    /// Execute a [`SubprocessCall`], check exit code, and parse stdout
    /// as JSON.
    pub async fn exec_parse<T: DeserializeOwned>(
        &self,
        call: &SubprocessCall,
    ) -> Result<T, ToolError> {
        let output = cli_runner::exec(call).await?;
        if output.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "{}: {}",
                output.command, output.stderr
            )));
        }
        serde_json::from_str(&output.stdout)
            .map_err(|e| ToolError::ExecutionFailed(format!("{}: {e}", output.command)))
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn prepare_gh_sets_token_and_env() {
        let (cli, repo) = stub_gh_cli_with_repo();
        let cwd = cli.resolve_repo_dir(&repo).unwrap();
        let call = cli.prepare_gh(&["pr", "view"], &cwd);
        assert_eq!(call.binary, "gh");
        assert_eq!(call.args, ["pr", "view"]);
        assert!(call.has_env("GH_TOKEN"));
        assert!(call.has_env("GH_PROMPT_DISABLED"));
        assert!(call.has_env("NO_COLOR"));
    }
}
