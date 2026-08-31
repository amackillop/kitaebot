//! `github_pr_diff_reply` tool — reply to an inline review comment.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{GithubApi, Tool, ToolCtx};
use crate::error::ToolError;
use crate::tools::string_or_value_required;

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
    #[serde(deserialize_with = "string_or_value_required")]
    pr_number: u64,
    /// ID of the review comment to reply to (from `pr_diff_comments`).
    #[serde(deserialize_with = "string_or_value_required")]
    comment_id: u64,
    /// Reply body (Markdown).
    body: String,
}

pub struct PrDiffReply(pub GithubApi);

impl Tool for PrDiffReply {
    fn name(&self) -> &'static str {
        "github_pr_diff_reply"
    }

    fn description(&self) -> &'static str {
        "Reply to an inline review comment"
    }

    fn parameters(&self) -> serde_json::Value {
        crate::tools::schema_of::<Args>()
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            self.run(&args).await
        })
    }
}

impl PrDiffReply {
    async fn run(&self, args: &Args) -> Result<String, ToolError> {
        let nwo = self.0.nwo(&args.repo_dir).await?;
        let reply = self
            .0
            .client()
            .reply_to_diff_comment(&nwo, args.pr_number, args.comment_id, &args.body)
            .await?;
        Ok(format!(
            "Reply posted on {nwo}#{} in thread {} (id {})",
            args.pr_number, args.comment_id, reply.id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::github::test_helpers::stub_api_with_repo;

    #[tokio::test]
    async fn replies_to_the_thread_endpoint() {
        let api = stub_api_with_repo("owner/repo", |method, path, body| {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/pulls/5/comments/123456/replies");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["body"], "Fixed in the latest push");
            br#"{"id":123999,"path":"a.rs","line":1,"body":"Fixed in the latest push",
                "user":{"login":"bot"},"created_at":"2026-01-01T00:00:00Z"}"#
                .to_vec()
        });
        let tool = PrDiffReply(api);
        let out = tool
            .run(&Args {
                repo_dir: "projects/r".into(),
                pr_number: 5,
                comment_id: 123_456,
                body: "Fixed in the latest push".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            out,
            "Reply posted on owner/repo#5 in thread 123456 (id 123999)"
        );
    }
}
