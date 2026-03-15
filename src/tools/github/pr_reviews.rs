//! `github_pr_reviews` tool — fetch review verdicts and PR comments.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use super::gh_cli::GhCli;
use super::types::PrReviewsResponse;
use crate::error::ToolError;
use crate::tools::cli_runner::SubprocessCall;

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

pub struct PrReviews(pub GhCli);

impl Tool for PrReviews {
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

impl PrReviews {
    fn prepare(&self, repo_dir: &str, pr_number: u64) -> Result<SubprocessCall, ToolError> {
        let cwd = self.0.resolve_repo_dir(repo_dir)?;
        let number = pr_number.to_string();
        Ok(self.0.prepare_gh(
            &[
                "pr",
                "view",
                &number,
                "--json",
                "reviews,reviewRequests,comments",
            ],
            &cwd,
        ))
    }

    /// Pure: format the review response for display.
    fn format_output(resp: &PrReviewsResponse, pr_number: u64) -> String {
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
            return format!("No reviews or comments on PR #{pr_number}.");
        }

        output
    }

    async fn run(&self, repo_dir: &str, pr_number: u64) -> Result<String, ToolError> {
        let call = self.prepare(repo_dir, pr_number)?;
        let resp: PrReviewsResponse = self.0.exec_parse(&call).await?;
        Ok(Self::format_output(&resp, pr_number))
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
    use super::*;
    use crate::tools::github::test_helpers::stub_gh_cli_with_repo;

    #[test]
    fn builds_correct_command() {
        let (gh, repo) = stub_gh_cli_with_repo();
        let tool = PrReviews(gh);
        let call = tool.prepare(&repo, 42).unwrap();
        assert_eq!(call.binary, "gh");
        assert!(call.args.contains(&"42".to_string()));
    }

    #[test]
    fn formats_reviews_and_comments() {
        let resp = PrReviewsResponse {
            reviews: vec![Review {
                author: Author {
                    login: "alice".to_string(),
                },
                body: "Looks good".to_string(),
                state: "APPROVED".to_string(),
                submitted_at: "2025-01-15T10:00:00Z".to_string(),
            }],
            review_requests: vec![ReviewRequest {
                login: Some("bob".to_string()),
                name: None,
            }],
            comments: vec![PrCommentEntry {
                author: Author {
                    login: "carol".to_string(),
                },
                body: "What about edge cases?".to_string(),
                created_at: "2025-01-15T11:00:00Z".to_string(),
            }],
        };
        let result = PrReviews::format_output(&resp, 42);
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

    #[test]
    fn empty() {
        let resp = PrReviewsResponse {
            reviews: vec![],
            review_requests: vec![],
            comments: vec![],
        };
        let result = PrReviews::format_output(&resp, 1);
        assert_eq!(result, "No reviews or comments on PR #1.");
    }
}
