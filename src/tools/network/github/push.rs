//! `git_push` tool — push commits to a remote.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::git_cli::GitCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

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

pub struct Push<R>(pub Arc<GitCli<R>>);

impl<R: CliRunner> Tool for Push<R> {
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

impl<R: CliRunner> Push<R> {
    async fn run(
        &self,
        repo_dir: &str,
        remote: Option<&str>,
        branch: Option<&str>,
        set_upstream: bool,
    ) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;

        let remote = remote.unwrap_or("origin");
        let mut args = vec!["push"];

        if set_upstream {
            args.push("--set-upstream");
        }

        args.push(remote);

        if let Some(b) = branch {
            args.push(b);
        }

        self.0.run_git(&args, &cwd, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::network::github::test_helpers::{ok_output, stub_git_arc_with_repo};

    #[tokio::test]
    async fn defaults_to_origin_authenticated() {
        let (git, repo, calls) = stub_git_arc_with_repo(vec![ok_output("Everything up-to-date")]);
        let tool = Push(git);
        let _ = tool.run(&repo, None, None, false).await.unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "git");
        assert_eq!(recorded[0].args, ["push", "origin"]);
        assert!(recorded[0].has_env("GIT_ASKPASS"));
    }

    #[tokio::test]
    async fn all_options_build_correct_args() {
        let (git, repo, calls) = stub_git_arc_with_repo(vec![ok_output("ok")]);
        let tool = Push(git);
        let _ = tool
            .run(&repo, Some("upstream"), Some("feat"), true)
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(
            recorded[0].args,
            ["push", "--set-upstream", "upstream", "feat"]
        );
    }
}
