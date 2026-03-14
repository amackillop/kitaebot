//! `github_pr_diff_reply` tool — reply to an inline review comment.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::CliRunner;

/// Reply to an inline review comment.
///
/// Use `pr_diff_comments` first to get comment IDs, then reply
/// to a specific one. This creates a threaded reply on the same
/// line/file, not a top-level PR comment.
#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
    /// ID of the review comment to reply to (from `pr_diff_comments`).
    comment_id: u64,
    /// Reply body (Markdown).
    body: String,
}

pub struct PrDiffReply<R>(pub Arc<GhCli<R>>);

impl<R: CliRunner> Tool for PrDiffReply<R> {
    fn name(&self) -> &'static str {
        "github_pr_diff_reply"
    }

    fn description(&self) -> &'static str {
        "Reply to an inline review comment"
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
            self.run(&args.repo_dir, args.pr_number, args.comment_id, &args.body)
                .await
        })
    }
}

impl<R: CliRunner> PrDiffReply<R> {
    async fn run(
        &self,
        repo_dir: &str,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let endpoint =
            format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments/{comment_id}/replies");
        let body_field = format!("body={body}");

        self.0
            .run_gh(&["api", &endpoint, "-f", &body_field], &cwd)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::network::github::test_helpers::{ok_output, stub_gh_arc_with_repo};

    #[tokio::test]
    async fn replies_to_correct_endpoint() {
        let (gh, repo, calls) = stub_gh_arc_with_repo(vec![ok_output("{}")]);
        let tool = PrDiffReply(gh);
        let _ = tool
            .run(&repo, 5, 123_456, "Fixed in the latest push")
            .await
            .unwrap();

        let recorded = calls.take().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].binary, "gh");
        assert_eq!(recorded[0].args[0], "api");
        assert_eq!(
            recorded[0].args[1],
            "repos/{owner}/{repo}/pulls/5/comments/123456/replies"
        );
        assert_eq!(recorded[0].args[2], "-f");
        assert_eq!(recorded[0].args[3], "body=Fixed in the latest push");
    }
}
