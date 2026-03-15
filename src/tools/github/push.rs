//! `git_push` tool — push commits to a remote.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root
    /// (e.g. `"projects/myrepo"`).
    repo_dir: String,
    /// Remote name. Defaults to `"origin"`.
    remote: Option<String>,
    /// Branch to push. Defaults to the current branch.
    branch: Option<String>,
    /// Set upstream tracking (`--set-upstream`).
    #[serde(default)]
    set_upstream: bool,
}

pub struct Push(pub GitCli);

impl Tool for Push {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> &'static str {
        "Push commits to a remote"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(
                &args.repo_dir,
                args.remote.as_deref(),
                args.branch.as_deref(),
                args.set_upstream,
            )
            .await
        })
    }
}

impl Push {
    fn prepare(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let remote = remote.unwrap_or("origin");
        let mut args: Vec<&str> = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }
        args.push(remote);
        if let Some(b) = branch {
            args.push(b);
        }

        Ok(self.0.prepare_git(&args, &cwd))
    }

    async fn run(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, remote, branch, set_upstream)?;
        self.0.exec_git(call, true).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_git_cli_with_repo;

    #[test]
    fn defaults_to_origin() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Push(git);
        let call = tool.prepare(&repo, None, None, false).unwrap();
        assert_eq!(call.binary, "git");
        assert_eq!(call.args, ["push", "origin"]);
    }

    #[test]
    fn all_options_build_correct_args() {
        let (git, repo) = stub_git_cli_with_repo();
        let tool = Push(git);
        let call = tool
            .prepare(&repo, Some("upstream"), Some("feat"), true)
            .unwrap();
        assert_eq!(call.args, ["push", "--set-upstream", "upstream", "feat"]);
    }
}
