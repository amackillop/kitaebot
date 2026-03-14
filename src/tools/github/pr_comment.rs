//! `github_pr_comment` tool — add a comment to a pull request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
    /// Comment body (Markdown).
    body: String,
}

pub struct PrComment<R>(pub Arc<GhCli<R>>);

impl<R: CliRunner> Tool for PrComment<R> {
    fn name(&self) -> &'static str {
        "github_pr_comment"
    }

    fn description(&self) -> &'static str {
        "Add a comment to a pull request"
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
            self.run(&args.repo_dir, args.pr_number, &args.body).await
        })
    }
}

impl<R: CliRunner> PrComment<R> {
    async fn run(&self, repo_dir: &str, pr_number: u64, body: &str) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();
        self.0
            .run_gh(&["pr", "comment", &number, "--body", body], &cwd)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::{ok_output, stub_gh_arc_with_repo};

    #[tokio::test]
    async fn posts_comment_to_pr() {
        let (gh, repo, calls) = stub_gh_arc_with_repo(vec![ok_output("ok")]);
        let tool = PrComment(gh);
        let _ = tool.run(&repo, 7, "LGTM").await.unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "gh");
        assert_eq!(recorded[0].args, ["pr", "comment", "7", "--body", "LGTM"]);
        assert!(recorded[0].has_env("GH_TOKEN"));
    }
}
