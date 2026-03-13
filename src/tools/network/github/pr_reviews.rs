//! `github_pr_reviews` tool — fetch review verdicts and PR comments.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::api::GitHubApi;
use super::client::GitHubClient;
use super::types::PrReviewsResponse;
use crate::error::ToolError;

/// Fetch top-level review verdicts and PR conversation comments.
///
/// Returns review approvals/rejections and top-level PR comments only.
/// Does NOT return inline code comments on specific lines — use
/// `github_pr_diff_comments` for those.
#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Repository directory relative to workspace root.
    repo_dir: String,
    /// PR number.
    pr_number: u64,
}

pub struct PrReviews<A>(pub Arc<GitHubClient<A>>);

impl<A: GitHubApi> Tool for PrReviews<A> {
    fn name(&self) -> &'static str {
        "github_pr_reviews"
    }

    fn description(&self) -> &'static str {
        "Fetch top-level review verdicts and PR conversation comments"
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
            self.run(&args.repo_dir, args.pr_number).await
        })
    }
}

impl<A: GitHubApi> PrReviews<A> {
    async fn run(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();

        let resp: PrReviewsResponse = self
            .0
            .run_gh_json(
                &["pr", "view", &number],
                "reviews,reviewRequests,comments",
                &cwd,
            )
            .await?;

        let mut output = String::new();

        if !resp.review_requests.is_empty() {
            output.push_str("Pending reviewers: ");
            let names: Vec<&str> = resp
                .review_requests
                .iter()
                .map(|r| {
                    r.login
                        .as_deref()
                        .or(r.name.as_deref())
                        .unwrap_or("unknown")
                })
                .collect();
            output.push_str(&names.join(", "));
            output.push_str("\n\n");
        }

        for r in &resp.reviews {
            let _ = writeln!(
                output,
                "@{} {} ({})",
                r.author.login, r.state, r.submitted_at
            );
            if !r.body.is_empty() {
                let _ = writeln!(output, "{}", r.body);
            }
            output.push('\n');
        }

        for c in &resp.comments {
            let _ = writeln!(output, "@{} ({})\n{}", c.author.login, c.created_at, c.body);
            output.push('\n');
        }

        if output.is_empty() {
            return Ok(format!("No reviews or comments on PR #{pr_number}."));
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{ok_output, stub_arc_with_repo};
    use super::*;

    #[test]
    fn deserialize() {
        let json = serde_json::json!({
            "repo_dir": "projects/myrepo",
            "pr_number": 42
        });
        let args: Args = serde_json::from_value(json).unwrap();
        assert_eq!(args.pr_number, 42);
    }

    #[tokio::test]
    async fn formats_reviews_and_comments() {
        let json = serde_json::to_string(&serde_json::json!({
            "reviews": [{
                "author": {"login": "alice"},
                "body": "Looks good",
                "state": "APPROVED",
                "submittedAt": "2025-01-15T10:00:00Z"
            }],
            "reviewRequests": [{"login": "bob", "name": null}],
            "comments": [{
                "author": {"login": "carol"},
                "body": "What about edge cases?",
                "createdAt": "2025-01-15T11:00:00Z"
            }]
        }))
        .unwrap();

        let (client, repo) = stub_arc_with_repo(vec![ok_output(&json)]);
        let tool = PrReviews(client);
        let result = tool.run(&repo, 42).await.unwrap();
        assert_eq!(
            result,
            "\
Pending reviewers: bob

@alice APPROVED (2025-01-15T10:00:00Z)
Looks good

@carol (2025-01-15T11:00:00Z)
What about edge cases?

"
        );
    }

    #[tokio::test]
    async fn empty() {
        let json = serde_json::to_string(&serde_json::json!({
            "reviews": [],
            "reviewRequests": [],
            "comments": []
        }))
        .unwrap();

        let (client, repo) = stub_arc_with_repo(vec![ok_output(&json)]);
        let tool = PrReviews(client);
        let result = tool.run(&repo, 1).await.unwrap();
        assert_eq!(result, "No reviews or comments on PR #1.");
    }
}
