//! `github_pr_diff_reply` tool — reply to an inline review comment.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use crate::error::ToolError;
use crate::tools::cli_runner::{self, SubprocessCall};

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

pub struct PrDiffReply(pub GhCli);

impl Tool for PrDiffReply {
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

impl PrDiffReply {
    fn prepare(
        &self,
        repo_dir: &str,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let endpoint =
            format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments/{comment_id}/replies");
        let body_field = format!("body={body}");
        Ok(self
            .0
            .prepare_gh(&["api", &endpoint, "-f", &body_field], &cwd))
    }

    async fn run(
        &self,
        repo_dir: &str,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, pr_number, comment_id, body)?;
        cli_runner::exec(&call).await?.format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn replies_to_correct_endpoint() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrDiffReply(gh);
        let call = tool
            .prepare(&repo, 5, 123_456, "Fixed in the latest push")
            .unwrap();
        assert_eq!(call.binary, "gh");
        assert_eq!(call.args[0], "api");
        assert_eq!(
            call.args[1],
            "repos/{owner}/{repo}/pulls/5/comments/123456/replies"
        );
        assert_eq!(call.args[2], "-f");
        assert_eq!(call.args[3], "body=Fixed in the latest push");
    }
}
