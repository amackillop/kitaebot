//! `gh` CLI subprocess wrapper.
//!
//! [`GhCli`] owns the token and workspace root needed by the
//! `github_gh` escape hatch. Auth injects `GH_TOKEN` into the
//! subprocess environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ToolError;
use crate::secrets::Secret;
use crate::tools::cli_runner::SubprocessCall;

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
            stdin: None,
        }
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
